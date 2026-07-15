use std::sync::Arc;

use futures::{StreamExt, pin_mut};
use ruma::{Mxc, OwnedMxcUri, OwnedUserId, UserId, http_headers::ContentDisposition};
use serde::Deserialize;
#[cfg(feature = "url_preview")]
use serde::Serialize;
use tuwunel_core::{
	Err, Result, at, debug, debug_info, err,
	utils::{
		ReadyExt, str_from_bytes,
		stream::{TryExpect, TryIgnore},
		string_from_bytes,
	},
};
use tuwunel_database::{Cbor, Database, Deserialized, Ignore, Interfix, Map, serialize_key};

use super::{Media, preview::CachedPreview, thumbnail::Dim};

pub(crate) struct Data {
	mediaid_file: Arc<Map>,
	mediaid_lazy: Arc<Map>,
	mediaid_lazycontent: Arc<Map>,
	mediaid_pending: Arc<Map>,
	mediaid_user: Arc<Map>,
	url_preview: Arc<Map>,
}

#[derive(Debug)]
pub struct Metadata {
	pub content_disposition: Option<ContentDisposition>,
	pub content_type: Option<String>,
	pub(super) key: Vec<u8>,
}

/// Borrowed staging-cache value: written zero-copy from the measured bytes.
#[cfg(feature = "url_preview")]
#[derive(Serialize)]
struct LazyContentRef<'a> {
	content_type: Option<&'a str>,
	content_disposition: Option<&'a str>,
	#[serde(with = "serde_bytes")]
	content: &'a [u8],
}

/// Owned staging-cache value read back at promotion. `ContentDisposition` is
/// Serialize-only, so the disposition rides as its header string.
#[derive(Deserialize)]
struct LazyContent {
	content_type: Option<String>,
	content_disposition: Option<String>,
	#[serde(with = "serde_bytes")]
	content: Vec<u8>,
}

impl From<LazyContent> for Media {
	fn from(lazy: LazyContent) -> Self {
		let content_disposition = lazy
			.content_disposition
			.and_then(|disposition| disposition.parse().ok());

		Self {
			content: lazy.content,
			content_type: lazy.content_type,
			content_disposition,
		}
	}
}

impl Data {
	pub(super) fn new(db: &Arc<Database>) -> Self {
		Self {
			mediaid_file: db["mediaid_file"].clone(),
			mediaid_lazy: db["mediaid_lazy"].clone(),
			mediaid_lazycontent: db["mediaid_lazycontent"].clone(),
			mediaid_pending: db["mediaid_pending"].clone(),
			mediaid_user: db["mediaid_user"].clone(),
			url_preview: db["url_preview"].clone(),
		}
	}

	pub(super) fn create_file_metadata(
		&self,
		mxc: &Mxc<'_>,
		user: Option<&UserId>,
		dim: &Dim,
		content_disposition: Option<&ContentDisposition>,
		content_type: Option<&str>,
	) -> Result<Vec<u8>> {
		let dim: &[u32] = &[dim.width, dim.height];
		let key = (mxc, dim, content_disposition, content_type);
		let key = serialize_key(key)?;
		self.mediaid_file.insert(&key, []);
		if let Some(user) = user {
			let key = (mxc, user);
			self.mediaid_user.put_raw(key, user);
		}

		Ok(key.to_vec())
	}

	/// Insert a pending MXC URI into the database
	pub(super) fn insert_pending_mxc(
		&self,
		mxc: &Mxc<'_>,
		user: &UserId,
		unused_expires_at: u64,
	) {
		let value = (unused_expires_at, user);
		debug!(?mxc, ?user, ?unused_expires_at, "Inserting pending");

		self.mediaid_pending
			.raw_put(mxc.to_string(), value);
	}

	/// Remove a pending MXC URI from the database
	pub(super) fn remove_pending_mxc(&self, mxc: &Mxc<'_>) {
		self.mediaid_pending.remove(&mxc.to_string());
	}

	/// Count the number of pending MXC URIs for a specific user
	pub(super) async fn count_pending_mxc_for_user(&self, user_id: &UserId) -> (usize, u64) {
		type KeyVal<'a> = (Ignore, (u64, &'a UserId));

		self.mediaid_pending
			.stream()
			.expect_ok()
			.ready_filter(|(_, (_, pending_user_id)): &KeyVal<'_>| user_id == *pending_user_id)
			.ready_fold(
				(0_usize, u64::MAX),
				|(count, earliest_expiration), (_, (expires_at, _))| {
					(count.saturating_add(1), earliest_expiration.min(expires_at))
				},
			)
			.await
	}

	/// Search for a pending MXC URI in the database
	pub(super) async fn search_pending_mxc(&self, mxc: &Mxc<'_>) -> Result<(OwnedUserId, u64)> {
		type Value<'a> = (u64, OwnedUserId);

		self.mediaid_pending
			.get(&mxc.to_string())
			.await
			.deserialized()
			.map(|(expires_at, user_id): Value<'_>| (user_id, expires_at))
			.inspect(|(user_id, expires_at)| debug!(?mxc, ?user_id, ?expires_at, "Found pending"))
			.map_err(|e| err!(Request(NotFound("Pending not found or error: {e}"))))
	}

	/// Map a minted mxc:// URI to the external URL it resolves to on first
	/// download (see `Service::fetch_lazy_media`).
	#[cfg(feature = "url_preview")]
	pub(super) fn insert_lazy_media(&self, mxc: &Mxc<'_>, url: &str) {
		debug!(?mxc, ?url, "Registering lazy media");

		self.mediaid_lazy
			.insert(&mxc.to_string(), url.as_bytes());
	}

	/// Remove a lazy media reference by its mxc:// URI string, unregistering
	/// the mxc.
	pub(super) fn remove_lazy_media(&self, mxc: &str) { self.mediaid_lazy.remove(mxc); }

	/// Look up the external URL a lazy media MXC URI refers to.
	pub(super) async fn search_lazy_media(&self, mxc: &Mxc<'_>) -> Result<String> {
		let handle = self.mediaid_lazy.get(&mxc.to_string()).await?;

		string_from_bytes(&handle)
			.map_err(|e| err!(Database(error!(?mxc, "Lazy media URL is invalid: {e}"))))
	}

	/// Stage the measured preview media bytes under its minted mxc so the
	/// first client download can promote without touching the origin.
	#[cfg(feature = "url_preview")]
	pub(super) fn set_lazy_content(
		&self,
		mxc: &str,
		content_type: Option<&str>,
		content_disposition: Option<&str>,
		content: &[u8],
	) {
		let value = LazyContentRef {
			content_type,
			content_disposition,
			content,
		};

		self.mediaid_lazycontent
			.raw_put(mxc, Cbor(&value));
	}

	/// Take the staged bytes a preview seeded for a lazy media mxc, if any.
	pub(super) async fn get_lazy_content(&self, mxc: &str) -> Result<Media> {
		self.mediaid_lazycontent
			.get(mxc)
			.await
			.deserialized::<Cbor<LazyContent>>()
			.map(at!(0))
			.map(Into::into)
	}

	pub(super) fn remove_lazy_content(&self, mxc: &str) { self.mediaid_lazycontent.remove(mxc); }

	pub(super) async fn delete_file_mxc(&self, mxc: &Mxc<'_>) {
		debug!("MXC URI: {mxc}");

		let prefix = (mxc, Interfix);
		self.mediaid_file
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| self.mediaid_file.remove(key))
			.await;

		self.mediaid_user
			.stream_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|(key, val)| {
				debug_assert!(
					key.starts_with(mxc.to_string().as_bytes()),
					"key should start with the mxc"
				);

				let user = str_from_bytes(val).unwrap_or_default();
				debug_info!("Deleting key {key:?} which was uploaded by user {user}");

				self.mediaid_user.remove(key);
			})
			.await;
	}

	/// Searches for all files with the given MXC
	pub(super) async fn search_mxc_metadata_prefix(&self, mxc: &Mxc<'_>) -> Result<Vec<Vec<u8>>> {
		debug!("MXC URI: {mxc}");

		let prefix = (mxc, Interfix);
		let keys: Vec<Vec<u8>> = self
			.mediaid_file
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.map(<[u8]>::to_vec)
			.collect()
			.await;

		if keys.is_empty() {
			return Err!(Database("Failed to find any keys in database for `{mxc}`",));
		}

		debug!("Got the following keys: {keys:?}");

		Ok(keys)
	}

	pub(super) async fn file_metadata_exists(&self, mxc: &Mxc<'_>, dim: &Dim) -> bool {
		let dim: &[u32] = &[dim.width, dim.height];
		let prefix = (mxc, dim, Interfix);
		let keys = self
			.mediaid_file
			.keys_prefix_raw(&prefix)
			.ignore_err();

		pin_mut!(keys);
		keys.next().await.is_some()
	}

	pub(super) async fn search_file_metadata(
		&self,
		mxc: &Mxc<'_>,
		dim: &Dim,
	) -> Result<Metadata> {
		let dim: &[u32] = &[dim.width, dim.height];
		let prefix = (mxc, dim, Interfix);

		let keys = self
			.mediaid_file
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.map(ToOwned::to_owned);

		pin_mut!(keys);
		let key = keys
			.next()
			.await
			.ok_or_else(|| err!(Request(NotFound("Media not found"))))?;

		let mut parts = key.rsplit(|&b| b == 0xFF);

		let content_type = parts
			.next()
			.map(string_from_bytes)
			.transpose()
			.map_err(|e| err!(Database(error!(?mxc, "Content-type is invalid: {e}"))))?;

		let content_disposition = parts
			.next()
			.map(Some)
			.ok_or_else(|| err!(Database(error!(?mxc, "Media ID in db is invalid."))))?
			.filter(|bytes| !bytes.is_empty())
			.map(string_from_bytes)
			.transpose()
			.map_err(|e| err!(Database(error!(?mxc, "Content-disposition is invalid: {e}"))))?
			.as_deref()
			.map(str::parse)
			.transpose()
			.map_err(|e| err!(Database(error!(?mxc, "Content-disposition is invalid: {e}"))))?;

		Ok(Metadata { content_disposition, content_type, key })
	}

	/// Gets all the MXCs associated with a user
	pub(super) async fn get_all_user_mxcs(&self, user_id: &UserId) -> Vec<OwnedMxcUri> {
		self.mediaid_user
			.stream()
			.ignore_err()
			.ready_filter_map(|(key, user): (&str, &UserId)| {
				(user == user_id).then(|| key.into())
			})
			.collect()
			.await
	}

	/// Gets all the media keys in our database (this includes all the metadata
	/// associated with it such as width, height, content-type, etc)
	pub(crate) async fn get_all_media_keys(&self) -> Vec<Vec<u8>> {
		self.mediaid_file
			.raw_keys()
			.ignore_err()
			.map(<[u8]>::to_vec)
			.collect()
			.await
	}

	pub(super) fn set_url_preview(&self, url: &str, cached: &CachedPreview) -> Result {
		self.url_preview.raw_put(url, Cbor(cached));

		Ok(())
	}

	pub(super) async fn get_url_preview(&self, url: &str) -> Result<CachedPreview> {
		self.url_preview
			.get(url)
			.await
			.deserialized::<Cbor<_>>()
			.map(at!(0))
			.ok()
			.filter(CachedPreview::valid)
			.ok_or(err!(Request(NotFound("Expired from cache"))))
	}
}

#[cfg(feature = "url_preview")]
#[cfg(test)]
mod tests {
	use minicbor_serde::{from_slice, to_vec};

	use super::{LazyContent, LazyContentRef, Media};

	#[test]
	fn lazy_content_roundtrip() {
		let content: &[u8] = b"\x00\x01\xFF\xFE arbitrary staged bytes";
		let value = LazyContentRef {
			content_type: Some("image/png"),
			content_disposition: Some("inline; filename=\"cat.png\""),
			content,
		};

		let bytes = to_vec(&value).expect("encodes");
		let decoded: LazyContent = from_slice(&bytes).expect("decodes");

		assert_eq!(decoded.content_type.as_deref(), Some("image/png"));
		assert_eq!(decoded.content.as_slice(), content);

		let media = Media::from(decoded);
		assert_eq!(media.content.as_slice(), content);
		assert!(media.content_disposition.is_some(), "disposition re-parses to the ruma type");
	}

	#[test]
	fn lazy_content_bytes_compact() {
		let content = vec![0xAB_u8; 4096];
		let value = LazyContentRef {
			content_type: None,
			content_disposition: None,
			content: content.as_slice(),
		};

		let bytes = to_vec(&value).expect("encodes");

		// serde_bytes must encode a CBOR byte string, not an array-of-uints
		// (~1.9x); only a small fixed header of overhead is permitted
		assert!(bytes.len() <= content.len() + 64);
	}
}
