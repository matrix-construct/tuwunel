//! Original-media location resolution without fetching or materializing media.

#[cfg(test)]
mod tests;

use std::{collections::HashSet, time::Duration};

use futures::{StreamExt, stream};
use ruma::OwnedMxcUri;
use serde::Serialize;
use tokio::time::{Instant, timeout_at};
use tuwunel_core::{Error, Result, config::StorageProvider, err, implement};

use super::{Animate, Dim, Service};

/// The result for one requested media URI.
#[derive(Debug, Serialize)]
pub struct MediaObjectLocations {
	/// The requested URI, preserved even when invalid.
	pub mxc_uri: String,
	/// Summary of the selected providers' results.
	pub status: MediaObjectStatus,
	/// One result for each selected provider when original metadata exists.
	pub objects: Vec<ProviderObjectLocation>,
}

/// Summary of an original-media location lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaObjectStatus {
	/// At least one selected object-store provider confirmed presence.
	Found,
	/// Original-media metadata is absent, including unmaterialized lazy media.
	NotFound,
	/// Original-media metadata could not be read or decoded.
	MetadataUnavailable,
	/// The supplied string is not a valid MXC URI.
	InvalidMxc,
	/// At least one candidate was resolved without verification.
	Unverified,
	/// No selected object-store provider has the object.
	NotPresent,
	/// At least one provider could not be inspected; presence is inconclusive.
	ProviderUnavailable,
	/// None of the selected providers supports object-store transfers.
	UnsupportedBackend,
}

/// A safe transfer locator and its presence in one selected provider.
#[derive(Debug, Serialize)]
pub struct ProviderObjectLocation {
	/// Exact configured provider identifier.
	pub provider: String,
	/// Logical object name without the provider's configured prefix.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub object_key_name: Option<String>,
	/// Verification outcome for this provider.
	pub presence: ObjectPresence,
	/// Byte length returned by HEAD, omitted for unverified or absent objects.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub size: Option<u64>,
}

/// Outcome of resolving or checking one selected media provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectPresence {
	/// The provider confirmed the object exists.
	Present,
	/// The provider confirmed the object is missing.
	NotPresent,
	/// Verification was disabled.
	Unverified,
	/// The provider could not be checked, including verification timeouts.
	ProviderUnavailable,
	/// Local filesystem storage has no object-store transfer address.
	UnsupportedBackend,
}

/// Selects providers using the same defaults as media reads, retaining disabled
/// identifiers so they can be reported rather than silently omitted.
#[implement(Service)]
pub fn object_location_providers(&self, requested: Option<&[String]>) -> Result<Vec<String>> {
	if requested.is_some_and(|providers| providers.is_empty() || providers.len() > 100) {
		return Err(err!(Request(InvalidParam("providers must contain 1 to 100 identifiers"))));
	}

	let configured = &self.services.config.media_storage_providers;
	let providers = requested.map_or_else(
		|| {
			if configured.is_empty() {
				self.services
					.storage
					.providers()
					.map(|p| p.name.clone())
					.collect()
			} else {
				configured.iter().cloned().collect()
			}
		},
		<[String]>::to_vec,
	);
	let mut seen = HashSet::new();

	Ok(providers
		.into_iter()
		.filter(|id| seen.insert(id.clone()))
		.collect())
}

/// Resolves at most 1,000 submitted URIs, deduplicating in request order. Each
/// item inspects every selected provider. At most 16 items run concurrently,
/// and a service-wide semaphore bounds provider HEAD requests across batches.
#[implement(Service)]
pub async fn object_locations(
	&self,
	mxc_uris: &[String],
	providers: &[String],
	verify: bool,
) -> Result<Vec<MediaObjectLocations>> {
	if mxc_uris.is_empty() || mxc_uris.len() > 1_000 {
		return Err(err!(Request(InvalidParam("mxc_uris must contain 1 to 1000 entries"))));
	}
	// Leave time for the remaining metadata lookups and response serialization
	// before the router's HTTP timeout, even when every provider stalls.
	let budget = Duration::from_secs(self.services.server.config.client_request_timeout)
		.checked_div(2)
		.unwrap_or_default()
		.min(Duration::from_secs(30));
	let now = Instant::now();
	let deadline = now.checked_add(budget).unwrap_or(now);

	let mut seen = HashSet::new();
	let uris = mxc_uris
		.iter()
		.filter(|uri| seen.insert(uri.as_str()))
		.cloned()
		.collect::<Vec<_>>();

	Ok(stream::iter(uris)
		.map(async |uri| {
			self.object_locations_one(&uri, providers, verify, deadline)
				.await
		})
		.buffered(16)
		.collect()
		.await)
}

#[implement(Service)]
async fn object_locations_one(
	&self,
	uri: &str,
	providers: &[String],
	verify: bool,
	deadline: Instant,
) -> MediaObjectLocations {
	let mut result = MediaObjectLocations {
		mxc_uri: uri.to_owned(),
		status: MediaObjectStatus::NotFound,
		objects: Vec::new(),
	};
	let mxc: OwnedMxcUri = uri.into();
	let Ok(parts) = mxc.parts() else {
		result.status = MediaObjectStatus::InvalidMxc;
		return result;
	};
	let metadata = match self
		.db
		.search_file_metadata(&parts, &Dim::default(), Animate::Allowed)
		.await
	{
		| Ok(metadata) => metadata,
		| Err(error) if error.is_missing() => return result,
		| Err(_) => {
			result.status = MediaObjectStatus::MetadataUnavailable;
			return result;
		},
	};
	let path = self.get_media_name_sha256(&metadata.key);
	let remote = !self
		.services
		.globals
		.server_is_ours(parts.server_name);

	for id in providers {
		let mut object = ProviderObjectLocation {
			provider: id.clone(),
			object_key_name: None,
			presence: ObjectPresence::ProviderUnavailable,
			size: None,
		};

		if let Ok(provider) = self.services.storage.provider(id) {
			let supported =
				matches!(&provider.config, StorageProvider::s3(_) | StorageProvider::gcs(_));
			if !supported {
				object.presence = ObjectPresence::UnsupportedBackend;
			} else if verify || remote {
				let now = Instant::now();
				let check_deadline = now
					.checked_add(Duration::from_secs(5))
					.unwrap_or(now)
					.min(deadline);
				let check = async {
					let _permit = self
						.object_location_slots
						.acquire()
						.await
						.map_err(|error| err!("{error}"))?;
					provider.head(&path).await
				};
				if Instant::now() < check_deadline {
					match timeout_at(check_deadline, check).await {
						| Ok(Ok(metadata)) => {
							object.presence = ObjectPresence::Present;
							object.size = Some(metadata.size);
						},
						| Ok(Err(Error::ObjectStore(object_store::Error::NotFound {
							..
						}))) => object.presence = ObjectPresence::NotPresent,
						| _ => {},
					}
				}
			} else {
				object.presence = ObjectPresence::Unverified;
			}

			if supported && (!remote || object.presence == ObjectPresence::Present) {
				object.object_key_name = Some(path.clone());
			}
		}

		result.objects.push(object);
	}

	result.status = summarize(&result.objects);
	result
}

fn summarize(objects: &[ProviderObjectLocation]) -> MediaObjectStatus {
	[
		(ObjectPresence::Present, MediaObjectStatus::Found),
		(ObjectPresence::Unverified, MediaObjectStatus::Unverified),
		(ObjectPresence::ProviderUnavailable, MediaObjectStatus::ProviderUnavailable),
		(ObjectPresence::NotPresent, MediaObjectStatus::NotPresent),
	]
	.into_iter()
	.find_map(|(presence, status)| {
		objects
			.iter()
			.any(|o| o.presence == presence)
			.then_some(status)
	})
	.unwrap_or(MediaObjectStatus::UnsupportedBackend)
}
