use std::{sync::Arc, time::Duration};

use futures::{StreamExt, pin_mut};
use ruma::{Mxc, OwnedMxcUri, UserId, http_headers::ContentDisposition};
use tuwunel_core::{
	Err, Result, debug, debug_info, err,
	utils::{ReadyExt, str_from_bytes, stream::TryIgnore, string_from_bytes},
};
use tuwunel_database::{Database, Interfix, Map, serialize_key};

use super::{preview::UrlPreviewData, thumbnail::Dim};

pub(crate) struct Data {
	mediaid_file: Arc<Map>,
	mediaid_user: Arc<Map>,
	url_previews: Arc<Map>,
}

#[derive(Debug)]
pub(super) struct Metadata {
	pub(super) content_disposition: Option<ContentDisposition>,
	pub(super) content_type: Option<String>,
	pub(super) key: Vec<u8>,
}

impl Data {
	pub(super) fn new(db: &Arc<Database>) -> Self {
		Self {
			mediaid_file: db["mediaid_file"].clone(),
			mediaid_user: db["mediaid_user"].clone(),
			url_previews: db["url_previews"].clone(),
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
			.map_err(|e| err!(Database(error!(?mxc, "Content-Type is invalid: {e}"))))?;

		let content_disposition = parts
			.next()
			.map(Some)
			.ok_or_else(|| err!(Database(error!(?mxc, "Media ID in db is invalid."))))?
			.filter(|bytes| !bytes.is_empty())
			.map(string_from_bytes)
			.transpose()
			.map_err(|e| err!(Database(error!(?mxc, "Content-Disposition is invalid: {e}"))))?
			.as_deref()
			.map(str::parse)
			.transpose()?;

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

	#[inline]
	pub(super) fn remove_url_preview(&self, url: &str) -> Result {
		self.url_previews.remove(url.as_bytes());
		Ok(())
	}

	pub(super) fn set_url_preview(
		&self,
		url: &str,
		data: &UrlPreviewData,
		timestamp: Duration,
	) -> Result {
		let mut value = Vec::<u8>::new();
		value.extend_from_slice(&timestamp.as_secs().to_be_bytes());
		value.push(0xFF);
		value.extend_from_slice(
			data.title
				.as_ref()
				.map(String::as_bytes)
				.unwrap_or_default(),
		);
		value.push(0xFF);
		value.extend_from_slice(
			data.description
				.as_ref()
				.map(String::as_bytes)
				.unwrap_or_default(),
		);
		value.push(0xFF);
		value.extend_from_slice(
			data.image
				.as_ref()
				.map(String::as_bytes)
				.unwrap_or_default(),
		);
		value.push(0xFF);
		value.extend_from_slice(&data.image_size.unwrap_or(0).to_be_bytes());
		value.push(0xFF);
		value.extend_from_slice(&data.image_width.unwrap_or(0).to_be_bytes());
		value.push(0xFF);
		value.extend_from_slice(&data.image_height.unwrap_or(0).to_be_bytes());

		self.url_previews.insert(url.as_bytes(), &value);

		Ok(())
	}

	pub(super) async fn get_url_preview(&self, url: &str) -> Result<UrlPreviewData> {
		let values = self.url_previews.get(url).await?;

		let mut values = values.split(|&b| b == 0xFF);

		let _ts = values.next();
		/* if we ever decide to use timestamp, this is here.
		match values.next().map(|b| u64::from_be_bytes(b.try_into().expect("valid BE array"))) {
			Some(0) => None,
			x => x,
		};*/

		let title = match values
			.next()
			.and_then(|b| String::from_utf8(b.to_vec()).ok())
		{
			| Some(s) if s.is_empty() => None,
			| x => x,
		};
		let description = match values
			.next()
			.and_then(|b| String::from_utf8(b.to_vec()).ok())
		{
			| Some(s) if s.is_empty() => None,
			| x => x,
		};
		let image = match values
			.next()
			.and_then(|b| String::from_utf8(b.to_vec()).ok())
		{
			| Some(s) if s.is_empty() => None,
			| x => x,
		};
		let image_size = match values
			.next()
			.map(|b| usize::from_be_bytes(b.try_into().unwrap_or_default()))
		{
			| Some(0) => None,
			| x => x,
		};
		let image_width = match values
			.next()
			.map(|b| u32::from_be_bytes(b.try_into().unwrap_or_default()))
		{
			| Some(0) => None,
			| x => x,
		};
		let image_height = match values
			.next()
			.map(|b| u32::from_be_bytes(b.try_into().unwrap_or_default()))
		{
			| Some(0) => None,
			| x => x,
		};

		Ok(UrlPreviewData {
			title,
			description,
			image,
			image_size,
			image_width,
			image_height,
		})
	}
}
