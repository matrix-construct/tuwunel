pub mod blurhash;
mod data;
pub(super) mod migrations;
mod preview;
mod remote;
mod tests;
mod thumbnail;
use std::{
	collections::HashMap,
	path::PathBuf,
	sync::{Arc, Mutex},
	time::{Duration, Instant, SystemTime},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose};
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, pin_mut};
use http::StatusCode;
use ruma::{
	Mxc, OwnedMxcUri, OwnedUserId, UserId,
	api::client::error::{ErrorKind, RetryAfter},
	http_headers::ContentDisposition,
};
use tokio::{fs, sync::Notify};
use tuwunel_core::{
	Err, Error, Result, debug, debug_error, debug_info, debug_warn, err, trace,
	utils::{
		self, BoolExt, MutexMap,
		result::{LogDebugErr, LogErr},
		stream::{IterStream, TryReadyExt},
		time::now_millis,
	},
	warn,
};

use self::data::{Data, Metadata};
pub use self::thumbnail::Dim;
use crate::storage::Provider;

#[derive(Debug)]
pub struct Media {
	pub content: Vec<u8>,
	pub content_type: Option<String>,
	pub content_disposition: Option<ContentDisposition>,
}

/// For MSC2246
struct MXCState {
	/// Save the notifier for each pending media upload
	notifiers: Mutex<HashMap<OwnedMxcUri, Arc<Notify>>>,
	/// Save the ratelimiter for each user
	ratelimiter: Mutex<HashMap<OwnedUserId, (Instant, f64)>>,
}

pub struct Service {
	pub(super) db: Data,
	services: Arc<crate::services::OnceServices>,
	url_preview_mutex: MutexMap<String, ()>,
	federation_mutex: MutexMap<String, ()>,
	mxc_state: MXCState,
}

/// generated MXC ID (`media-id`) length
pub const MXC_LENGTH: usize = 32;

/// Cache control for immutable objects.
pub const CACHE_CONTROL_IMMUTABLE: &str = "private,max-age=31536000,immutable";

/// Default cross-origin resource policy.
pub const CORP_CROSS_ORIGIN: &str = "cross-origin";

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data::new(args.db),
			services: args.services.clone(),
			url_preview_mutex: MutexMap::new(),
			federation_mutex: MutexMap::new(),
			mxc_state: MXCState {
				notifiers: Mutex::new(HashMap::new()),
				ratelimiter: Mutex::new(HashMap::new()),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Create a pending media upload ID.
	#[tracing::instrument(level = "debug", skip(self))]
	pub async fn create_pending(
		&self,
		mxc: &Mxc<'_>,
		user: &UserId,
		unused_expires_at: u64,
	) -> Result {
		let config = &self.services.server.config;

		// Rate limiting (rc_media_create)
		let rate = f64::from(config.media_rc_create_per_second);
		let burst = f64::from(config.media_rc_create_burst_count);

		// Check rate limiting
		if rate > 0.0 && burst > 0.0 {
			let now = Instant::now();
			let mut ratelimiter = self.mxc_state.ratelimiter.lock()?;

			let (last_time, tokens) = ratelimiter
				.entry(user.to_owned())
				.or_insert_with(|| (now, burst));

			let elapsed = now.duration_since(*last_time).as_secs_f64();
			let new_tokens = elapsed.mul_add(rate, *tokens).min(burst);

			if new_tokens >= 1.0 {
				*last_time = now;
				*tokens = new_tokens - 1.0;
			} else {
				return Err(Error::Request(
					ErrorKind::LimitExceeded { retry_after: None },
					"Too many pending media creation requests.".into(),
					StatusCode::TOO_MANY_REQUESTS,
				));
			}
		}

		let max_uploads = config.max_pending_media_uploads;
		let (current_uploads, earliest_expiration) =
			self.db.count_pending_mxc_for_user(user).await;

		// Check if the user has reached the maximum number of pending media uploads
		if current_uploads >= max_uploads {
			let retry_after = earliest_expiration.saturating_sub(now_millis());
			return Err(Error::Request(
				ErrorKind::LimitExceeded {
					retry_after: Some(RetryAfter::Delay(Duration::from_millis(retry_after))),
				},
				"Maximum number of pending media uploads reached.".into(),
				StatusCode::TOO_MANY_REQUESTS,
			));
		}

		self.db
			.insert_pending_mxc(mxc, user, unused_expires_at);

		Ok(())
	}

	/// Uploads content to a pending media ID.
	#[tracing::instrument(level = "debug", skip(self))]
	pub async fn upload_pending(
		&self,
		mxc: &Mxc<'_>,
		user: &UserId,
		content_disposition: Option<&ContentDisposition>,
		content_type: Option<&str>,
		file: &[u8],
	) -> Result {
		let Ok((owner_id, expires_at)) = self.db.search_pending_mxc(mxc).await else {
			if self.get_metadata(mxc).await.is_some() {
				return Err!(Request(CannotOverwriteMedia("Media ID already has content")));
			}

			return Err!(Request(NotFound("Media not found")));
		};

		if owner_id != user {
			return Err!(Request(Forbidden("You did not create this media ID")));
		}

		let current_time = now_millis();
		if expires_at < current_time {
			return Err!(Request(NotFound("Pending media ID expired")));
		}

		self.create(mxc, Some(user), content_disposition, content_type, file)
			.await?;

		self.db.remove_pending_mxc(mxc);

		let mxc_uri: OwnedMxcUri = mxc.to_string().into();
		if let Some(notifier) = self
			.mxc_state
			.notifiers
			.lock()?
			.get(&mxc_uri)
			.cloned()
		{
			notifier.notify_waiters();
			self.mxc_state.notifiers.lock()?.remove(&mxc_uri);
		}

		Ok(())
	}

	/// Uploads a file.
	pub async fn create(
		&self,
		mxc: &Mxc<'_>,
		user: Option<&UserId>,
		content_disposition: Option<&ContentDisposition>,
		content_type: Option<&str>,
		file: &[u8],
	) -> Result {
		// Width, Height = 0 if it's not a thumbnail
		let key = self.db.create_file_metadata(
			mxc,
			user,
			&Dim::default(),
			content_disposition,
			content_type,
		)?;

		//TODO: Dangling metadata in database if creation fails
		self.create_media_file(&key, file).await
	}

	/// Deletes a file in the database and from the media directory via an MXC
	#[tracing::instrument(level = "trace", skip(self))]
	pub async fn delete(&self, mxc: &Mxc<'_>) -> Result {
		match self.db.search_mxc_metadata_prefix(mxc).await {
			| Ok(keys) => {
				for key in keys {
					trace!(?mxc, "MXC Key: {key:?}");
					debug_info!(?mxc, "Deleting from storage provider");

					if let Err(e) = self.remove_media_file(&key).await {
						debug_error!(?mxc, "Failed to remove media file: {e}");
					}

					debug_info!(?mxc, "Deleting from database");
					self.db.delete_file_mxc(mxc).await;
				}

				Ok(())
			},
			| _ => {
				Err!(Database(error!(
					"Failed to find any media keys for MXC {mxc} in our database."
				)))
			},
		}
	}

	/// Deletes all media by the specified user
	///
	/// currently, this is only practical for local users
	#[tracing::instrument(level = "trace", skip(self))]
	pub async fn delete_from_user(&self, user: &UserId) -> Result<usize> {
		let mxcs = self.db.get_all_user_mxcs(user).await;
		let mut deletion_count: usize = 0;

		for mxc in mxcs {
			let Ok(mxc) = mxc.as_str().try_into().inspect_err(|e| {
				debug_error!(?mxc, "Failed to parse MXC URI from database: {e}");
			}) else {
				continue;
			};

			debug_info!(
				%deletion_count,
				"Deleting MXC {mxc} by user {user} from database and filesystem",
			);
			match self.delete(&mxc).await {
				| Ok(()) => {
					deletion_count = deletion_count.saturating_add(1);
				},
				| Err(e) => {
					debug_error!(
						%deletion_count,
						"Failed to delete {mxc} from user {user}, ignoring error: {e}"
					);
				},
			}
		}

		Ok(deletion_count)
	}

	/// Get file from local storage or make a federation request if it
	/// originates remotely.
	#[tracing::instrument(
		level = "debug",
		err(level = "debug")
		skip(self),
	)]
	pub async fn get_or_fetch(
		&self,
		mxc: &Mxc<'_>,
		timeout_ms: Duration,
		user: &UserId,
	) -> Result<Media> {
		if let Ok(media) = self.get(mxc, Some(timeout_ms)).await {
			return Ok(media);
		}

		if self
			.services
			.globals
			.server_is_ours(mxc.server_name)
		{
			return Err!(Request(NotFound("Local media not found.")));
		}

		let lock = self.federation_mutex.lock(&mxc.to_string()).await;

		if self
			.db
			.file_metadata_exists(mxc, &Dim::default())
			.await
		{
			drop(lock);
			return self.get(mxc, None).await;
		}

		self.fetch_remote_content(mxc, Some(user), None, timeout_ms)
			.await
	}

	/// Get file from local storage while waiting up to a timeout_ms if it is
	/// pending.
	#[tracing::instrument(
		level = "debug",
		err(level = "trace")
		skip(self),
	)]
	pub async fn get(&self, mxc: &Mxc<'_>, timeout: Option<Duration>) -> Result<Media> {
		if let Ok(meta) = self.get_stored(mxc).await {
			return Ok(meta);
		}

		let Some(timeout) = timeout else {
			return Err!(Request(NotFound("Media not found.")));
		};

		let Ok(_pending) = self.db.search_pending_mxc(mxc).await else {
			return Err!(Request(NotFound("Media not found.")));
		};

		let notifier = self
			.mxc_state
			.notifiers
			.lock()?
			.entry(mxc.to_string().into())
			.or_insert_with(|| Arc::new(Notify::new()))
			.clone();

		if tokio::time::timeout(timeout, notifier.notified())
			.await
			.is_err()
		{
			return Err!(Request(NotYetUploaded("Media has not been uploaded yet")));
		}

		self.get_stored(mxc).await
	}

	/// Get file from local storage.
	#[tracing::instrument(level = "debug", skip(self))]
	pub async fn get_stored(&self, mxc: &Mxc<'_>) -> Result<Media> {
		let meta = self
			.db
			.search_file_metadata(mxc, &Dim::default())
			.await;

		let Ok(Metadata { content_type, content_disposition, key }) = meta else {
			return Err!(Request(NotFound("Media not found.")));
		};

		let path = self.get_media_name_sha256(&key);
		let fetch = self
			.storage_providers()
			.stream()
			.filter_map(async |provider| {
				provider
					.get(path.as_str())
					.await
					.log_debug_err()
					.ok()
			});

		pin_mut!(fetch);
		let Some(bytes) = fetch.next().await else {
			return Err!(Request(NotFound("Media not found.")));
		};

		Ok(Media {
			content: bytes.to_vec(),
			content_type,
			content_disposition,
		})
	}

	/// Gets all the MXC URIs in our media database
	pub async fn get_all_mxcs(&self) -> Result<Vec<OwnedMxcUri>> {
		let all_keys = self.db.get_all_media_keys().await;

		let mut mxcs = Vec::with_capacity(all_keys.len());

		for key in all_keys {
			trace!("Full MXC key from database: {key:?}");

			let mut parts = key.split(|&b| b == 0xFF);
			let mxc = parts
				.next()
				.map(|bytes| {
					utils::string_from_bytes(bytes).map_err(|e| {
						err!(Database(error!(
							"Failed to parse MXC unicode bytes from our database: {e}"
						)))
					})
				})
				.transpose()?;

			let Some(mxc_s) = mxc else {
				debug_warn!(
					?mxc,
					"Parsed MXC URL unicode bytes from database but is still invalid"
				);
				continue;
			};

			trace!("Parsed MXC key to URL: {mxc_s}");
			let mxc = OwnedMxcUri::from(mxc_s);

			if mxc.is_valid() {
				mxcs.push(mxc);
			} else {
				debug_warn!("{mxc:?} from database was found to not be valid");
			}
		}

		Ok(mxcs)
	}

	/// Deletes all media files before or after the given time. Returns a usize
	/// with the number of media files deleted.
	pub async fn delete_range(
		&self,
		time: SystemTime,
		older_than: bool,
		newer_than: bool,
		yes_i_want_to_delete_local_media: bool,
	) -> Result<usize> {
		let all_keys = self.db.get_all_media_keys().await;
		let mut remote_mxcs = Vec::with_capacity(all_keys.len());

		for key in all_keys {
			trace!("Full MXC key from database: {key:?}");
			let mut parts = key.split(|&b| b == 0xFF);
			let mxc = parts
				.next()
				.map(|bytes| {
					utils::string_from_bytes(bytes).map_err(|e| {
						err!(Database(error!(
							"Failed to parse MXC unicode bytes from our database: {e}"
						)))
					})
				})
				.transpose()?;

			let Some(mxc_s) = mxc else {
				debug_warn!(
					?mxc,
					"Parsed MXC URL unicode bytes from database but is still invalid"
				);
				continue;
			};

			trace!("Parsed MXC key to URL: {mxc_s}");
			let mxc = OwnedMxcUri::from(mxc_s);
			if (mxc.server_name() == Ok(self.services.globals.server_name())
				&& !yes_i_want_to_delete_local_media)
				|| !mxc.is_valid()
			{
				debug!("Ignoring local or broken media MXC: {mxc}");
				continue;
			}

			let file_created_at = if let Some(file_metadata) = self
				.storage_providers()
				.stream()
				.filter_map(async |provider| {
					let path = self.get_media_name_sha256(&key);
					match provider.head(&path).await {
						| Ok(file_metadata) => {
							trace!(%mxc, ?path, "Provider file metadata: {file_metadata:?}");
							Some(file_metadata)
						},
						| Err(e) => {
							debug_warn!(
								"Failed to obtain {:?} file metadata for MXC {mxc} at file path \
								 {path:?}\", skipping: {e}",
								provider.name,
							);
							None
						},
					}
				})
				.boxed()
				.next()
				.await
			{
				SystemTime::from(file_metadata.last_modified)
			} else {
				continue;
			};

			debug!("File created at: {file_created_at:?}");

			if file_created_at <= time && older_than {
				debug!(
					"File is older than user duration, pushing to list of file paths and keys \
					 to delete."
				);
				remote_mxcs.push(mxc.to_string());
			} else if file_created_at >= time && newer_than {
				debug!(
					"File is newer than user duration, pushing to list of file paths and keys \
					 to delete."
				);
				remote_mxcs.push(mxc.to_string());
			}
		}

		if remote_mxcs.is_empty() {
			return Err!(Database("Did not found any eligible MXCs to delete."));
		}

		debug_info!("Deleting media now in the past {time:?}");

		let mut deletion_count: usize = 0;

		for mxc in remote_mxcs {
			let Ok(mxc) = mxc.as_str().try_into() else {
				debug_warn!("Invalid MXC in database, skipping");
				continue;
			};

			debug_info!("Deleting MXC {mxc} from database and filesystem");

			match self.delete(&mxc).await {
				| Ok(()) => {
					deletion_count = deletion_count.saturating_add(1);
				},
				| Err(e) => {
					warn!("Failed to delete {mxc}, ignoring error and skipping: {e}");
					continue;
				},
			}
		}

		Ok(deletion_count)
	}

	pub async fn create_media_dir(&self) -> Result {
		let dir = self.get_media_dir();
		Ok(fs::create_dir_all(dir).await?)
	}

	async fn remove_media_file(&self, key: &[u8]) -> Result {
		let path = self.get_media_name_sha256(key);
		self.storage_providers()
			.stream()
			.filter_map(async |provider| {
				debug!(
					?key, ?path, provider = ?provider.name,
					"Deleting media file from provider",
				);

				provider
					.delete_one(&path)
					.await
					.log_debug_err()
					.ok()
			})
			.count()
			.map(|count| {
				count
					.ge(&0)
					.ok_or_else(|| err!(Request(NotFound("Failed to remove on any provider."))))
			})
			.await
	}

	async fn create_media_file(&self, key: &[u8], file: &[u8]) -> Result {
		self.storage_providers()
			.try_stream()
			.ready_try_filter(|provider| {
				let store_media_on_providers = &self.services.config.store_media_on_providers;

				store_media_on_providers.is_empty()
					|| store_media_on_providers.contains(&provider.name)
			})
			.and_then(async |provider| {
				let path = self.get_media_name_sha256(key);
				debug!(
					?key, ?path,
					len = ?file.len(),
					provider = ?provider.name,
					"Creating media file on storage provider."
				);

				provider
					.put_one(path.as_str(), file.to_vec())
					.await
					.log_err()?;

				Ok(1)
			})
			.ready_try_fold(0_usize, |a, c| Ok(a.saturating_add(c)))
			.inspect_ok(|&uploads| assert!(uploads > 0, "Successfully saved to nowhere."))
			.map_ok(|_| ())
			.await
	}

	fn storage_providers(&self) -> impl Iterator<Item = &Arc<Provider>> + Send + '_ {
		self.services
			.config
			.media_storage_providers
			.iter()
			.filter_map(|id| self.services.storage.provider(id).ok())
	}

	#[inline]
	pub async fn get_metadata(&self, mxc: &Mxc<'_>) -> Option<Metadata> {
		self.db
			.search_file_metadata(mxc, &Dim::default())
			.await
			.ok()
	}

	#[inline]
	#[must_use]
	pub fn get_media_path_sha256(&self, key: &[u8]) -> PathBuf {
		let mut r = self.get_media_dir();
		r.push(self.get_media_name_sha256(key));
		r
	}

	/// new SHA256 file name media function. requires database migrated. uses
	/// SHA256 hash of the base64 key as the file name
	#[inline]
	#[must_use]
	pub fn get_media_name_sha256(&self, key: &[u8]) -> String {
		// Using the hash of the base64 key as the filename prevents the total
		// length of the path from exceeding the maximum length in most
		// filesystems
		let digest = <sha2::Sha256 as sha2::Digest>::digest(key);
		encode_key(&digest)
	}

	/// old base64 file name media function
	/// This is the old version of `get_media_path_sha256` that uses the full
	/// base64 key as the filename.
	#[must_use]
	pub fn get_media_path_b64(&self, key: &[u8]) -> PathBuf {
		let mut r = self.get_media_dir();
		let encoded = encode_key(key);
		r.push(encoded);
		r
	}

	#[must_use]
	pub fn get_media_dir(&self) -> PathBuf {
		let mut r = PathBuf::new();
		r.push(self.services.server.config.database_path.clone());
		r.push("media");
		r
	}
}

#[inline]
#[must_use]
pub fn encode_key(key: &[u8]) -> String { general_purpose::URL_SAFE_NO_PAD.encode(key) }
