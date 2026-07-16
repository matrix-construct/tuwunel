mod data;
pub(super) mod migrations;
mod preview;
mod remote;
mod tests;
mod thumbnail;
use std::{
	collections::{HashMap, HashSet},
	path::PathBuf,
	sync::{Arc, Mutex},
	time::{Duration, Instant, SystemTime},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose};
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, pin_mut};
use http::StatusCode;
use object_store::ObjectMeta;
use ruma::{
	Mxc, OwnedMxcUri, OwnedUserId, UserId,
	api::error::{ErrorKind, RetryAfter},
	http_headers::ContentDisposition,
};
use tokio::{fs, sync::Notify};
use tuwunel_core::{
	Err, Error, Result, debug, debug_error, debug_info, debug_warn, err, trace,
	utils::{
		self, BoolExt, MutexMap,
		result::LogDebugErr,
		stream::{BroadbandExt, IterStream, ReadyExt, TryReadyExt},
		time::now_millis,
	},
	warn,
};
use url::Url;

use self::data::Data;
pub use self::{data::Metadata, preview::UrlPreviewData, thumbnail::Dim};
use crate::storage::Provider;

#[derive(Debug)]
pub struct Media {
	pub content: Vec<u8>,
	pub content_type: Option<String>,
	pub content_disposition: Option<ContentDisposition>,
}

/// One row of a user's uploaded media, holding only the fields tuwunel can
/// derive from the uploader index and storage-provider object metadata. The
/// uploader is carried when the index holds a row for it.
#[derive(Clone, Debug)]
pub struct UserMediaEntry {
	pub mxc: OwnedMxcUri,
	pub media_type: Option<String>,
	pub upload_name: Option<String>,
	pub media_length: Option<u64>,
	pub created_ts: u64,
	pub user_id: Option<OwnedUserId>,
}

/// One locally-uploaded media item's uploader and storage-object byte length
/// and modification time, the row shape of the media-statistics scan.
#[derive(Clone, Debug)]
pub struct UploadStat {
	pub user_id: OwnedUserId,
	pub media_length: u64,
	pub created_ts: u64,
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

/// Validity window for a presigned media download redirect (MSC3860).
const REDIRECT_TTL: Duration = Duration::from_mins(5);

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
					ErrorKind::LimitExceeded(ruma::api::error::LimitExceededErrorData {
						retry_after: None,
					}),
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
				ErrorKind::LimitExceeded(ruma::api::error::LimitExceededErrorData {
					retry_after: Some(RetryAfter::Delay(Duration::from_millis(retry_after))),
				}),
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
		let notifier = self.mxc_state.notifiers.lock()?.remove(&mxc_uri);

		if let Some(notifier) = notifier {
			notifier.notify_waiters();
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
		// lazy URL-preview media has no file keys of its own; drop its reference
		// and staged bytes whenever present so a delete can't re-mint the media
		let had_lazy = self.db.search_lazy_media(mxc).await.is_ok();
		if had_lazy {
			let key = mxc.to_string();
			self.db.remove_lazy_media(&key);
			self.db.remove_lazy_content(&key);
		}

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
			| _ if had_lazy => Ok(()),
			| _ => Err!(Database(error!(
				"Failed to find any media keys for MXC {mxc} in our database."
			))),
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
	pub async fn get_or_fetch(&self, mxc: &Mxc<'_>, timeout_ms: Duration) -> Result<Media> {
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

		self.fetch_remote_content(mxc, None, timeout_ms)
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
			return self.fetch_lazy_media(mxc).await;
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

	/// Resolve lazy URL-preview media on first download, promoting the staged
	/// or freshly-fetched bytes into the media store so the origin is fetched
	/// at most once per item.
	#[tracing::instrument(level = "debug", skip(self))]
	async fn fetch_lazy_media(&self, mxc: &Mxc<'_>) -> Result<Media> {
		let key = mxc.to_string();

		// bound outbound amplification to one in-flight fetch per mxc; requests
		// for the same mxc queue rather than fan out
		let _lock = self.federation_mutex.lock(&key).await;

		// a queued waiter for the same mxc may have promoted it already
		if self
			.db
			.search_file_metadata(mxc, &Dim::default())
			.await
			.is_ok()
		{
			// box the cold re-entry to break the get_stored/fetch_lazy_media cycle
			return Box::pin(self.get_stored(mxc)).await;
		}

		let media = match self.db.get_lazy_content(&key).await {
			| Ok(media) => media,
			| Err(_) => {
				let Ok(url) = self.db.search_lazy_media(mxc).await else {
					return Err!(Request(NotFound("Media not found.")));
				};

				let limit = self.services.config.url_preview_max_media_size;
				self.location_request(&self.services.client.url_preview_media, &url, limit)
					.await?
			},
		};

		// promote through the ordinary upload path so real file metadata makes
		// every later download and thumbnail a normal-media hit
		if let Err(e) = self
			.create(
				mxc,
				None,
				media.content_disposition.as_ref(),
				media.content_type.as_deref(),
				&media.content,
			)
			.await
		{
			// a failed promotion leaves metadata without bytes, masking the lazy
			// fallback on every later read; drop it so the next read retries
			self.db.delete_file_mxc(mxc).await;

			return Err(e);
		}

		self.db.remove_lazy_media(&key);
		self.db.remove_lazy_content(&key);

		Ok(media)
	}

	/// Presigned redirect URL for locally-stored media (MSC3860).
	///
	/// Returns the first configured provider's signed URL for the object, or
	/// `None` when redirects are disabled, the media is unknown, or no provider
	/// can presign (filesystem-only media).
	#[tracing::instrument(level = "debug", skip(self))]
	pub async fn redirect_url(&self, mxc: &Mxc<'_>, dim: &Dim) -> Result<Option<Url>> {
		if !self.services.config.media_allow_redirect {
			return Ok(None);
		}

		let Ok(Metadata { key, .. }) = self.db.search_file_metadata(mxc, dim).await else {
			return Ok(None);
		};

		let path = self.get_media_name_sha256(&key);
		let urls = self
			.storage_providers()
			.stream()
			.filter_map(async |provider| {
				provider
					.signed_get_url(path.as_str(), REDIRECT_TTL)
					.await
					.log_debug_err()
					.ok()
					.flatten()
			});

		pin_mut!(urls);

		Ok(urls.next().await)
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

	/// Every media item uploaded by a local user, carrying the fields tuwunel
	/// can derive: content type, upload name, byte length and modification time
	/// from storage-provider object metadata. Untracked columns (last-access,
	/// quarantine, url-cache) are not represented.
	#[tracing::instrument(level = "debug", skip(self))]
	pub async fn user_media(&self, user: &UserId) -> Result<Vec<UserMediaEntry>> {
		let entries = self
			.db
			.get_all_user_mxcs(user)
			.await
			.into_iter()
			.stream()
			.broad_filter_map(async |mxc| self.user_media_entry(Some(user), mxc).await)
			.collect()
			.await;

		Ok(entries)
	}

	/// Derivable metadata for the single media item at the given MXC, with the
	/// uploading local user resolved from the uploader index when present.
	#[tracing::instrument(level = "debug", skip(self))]
	pub async fn media_entry(&self, mxc: &Mxc<'_>) -> Option<UserMediaEntry> {
		let user = self.db.mxc_user(mxc).await;

		self.user_media_entry(user.as_deref(), mxc.to_string().into())
			.await
	}

	async fn user_media_entry(
		&self,
		user: Option<&UserId>,
		mxc: OwnedMxcUri,
	) -> Option<UserMediaEntry> {
		let parts = mxc.parts().ok()?;
		let Metadata { content_type, content_disposition, key } =
			self.get_metadata(&parts).await?;

		let object = self.head_meta(&key).await;
		let upload_name = content_disposition.and_then(|disposition| disposition.filename);

		Some(UserMediaEntry {
			media_type: content_type,
			upload_name,
			media_length: object.as_ref().map(|object| object.size),
			created_ts: object.as_ref().map(mtime_millis).unwrap_or(0),
			user_id: user.map(ToOwned::to_owned),
			mxc,
		})
	}

	/// Uploader, byte length and storage modification time of every media item
	/// uploaded by a local user, one row per upload; media missing from every
	/// storage provider are skipped.
	pub fn upload_stats(&self) -> impl Stream<Item = UploadStat> + Send + '_ {
		self.db
			.all_uploads()
			.broad_filter_map(async |(mxc, user_id)| {
				let parts = mxc.parts().ok()?;
				let Metadata { key, .. } = self.get_metadata(&parts).await?;
				let object = self.head_meta(&key).await?;

				Some(UploadStat {
					user_id,
					media_length: object.size,
					created_ts: mtime_millis(&object),
				})
			})
	}

	/// Deletes local media older than `before_ts` (by storage-provider mtime)
	/// and strictly larger than `size_gt` bytes, sparing profile and
	/// room-avatar media when `keep_profiles`. Returns the deleted MXCs, empty
	/// when none match. Quarantine and protection flags are not tracked, so no
	/// media is spared on those grounds.
	#[tracing::instrument(level = "debug", skip(self))]
	pub async fn delete_by_date_size(
		&self,
		before_ts: u64,
		size_gt: u64,
		keep_profiles: bool,
	) -> Result<Vec<OwnedMxcUri>> {
		let spared = keep_profiles
			.then_async(|| self.avatar_mxcs())
			.await
			.unwrap_or_default();

		let candidates = self.get_all_mxcs().await?;

		let deleted: Vec<OwnedMxcUri> = candidates
			.into_iter()
			.stream()
			.ready_filter(|mxc| self.is_local(mxc) && !spared.contains(mxc))
			.broad_filter_map(async |mxc| {
				let parts = mxc.parts().ok()?;
				let Metadata { key, .. } = self.get_metadata(&parts).await?;
				let object = self.head_meta(&key).await?;

				let eligible = mtime_millis(&object) < before_ts && object.size > size_gt;

				eligible
					.then_async(|| self.delete(&parts))
					.await
					.and_then(Result::ok)
					.map(|()| mxc)
			})
			.collect()
			.await;

		Ok(deleted)
	}

	/// The MXCs of every local user's profile avatar and every room's avatar,
	/// the spare-set honoured by `keep_profiles`.
	async fn avatar_mxcs(&self) -> HashSet<OwnedMxcUri> {
		let user_avatars = self
			.services
			.users
			.list_local_users()
			.map(ToOwned::to_owned)
			.broad_filter_map(async |user| self.services.profile.avatar_url(&user).await.ok());

		let room_avatars = self
			.services
			.metadata
			.iter_ids()
			.map(ToOwned::to_owned)
			.broad_filter_map(async |room_id| {
				self.services
					.state_accessor
					.get_avatar(&room_id)
					.await
					.ok()
					.and_then(|avatar| avatar.url)
			});

		user_avatars.chain(room_avatars).collect().await
	}

	fn is_local(&self, mxc: &OwnedMxcUri) -> bool {
		mxc.server_name()
			.is_ok_and(|server| self.services.globals.server_is_ours(server))
	}

	/// First storage provider's object metadata for the media stored under
	/// `key` (byte length and modification time), or `None` when no provider
	/// holds it.
	async fn head_meta(&self, key: &[u8]) -> Option<ObjectMeta> {
		let path = self.get_media_name_sha256(key);

		let stream = self
			.storage_providers()
			.stream()
			.filter_map(async |provider| provider.head(&path).await.log_debug_err().ok());

		pin_mut!(stream);
		stream.next().await
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
					.into_option()
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

				if let Err(e) = provider
					.put_one(path.as_str(), file.to_vec())
					.await
				{
					return Err!(Database(error!(
						?path,
						?provider,
						"Failed to store media on provider: {e:?}"
					)));
				}

				Ok(1)
			})
			.ready_try_fold(0_usize, |a, c| Ok(a.saturating_add(c)))
			.inspect_ok(|&uploads| assert!(uploads > 0, "Successfully saved to nowhere."))
			.map_ok(|_| ())
			.await
	}

	fn storage_providers(&self) -> impl Iterator<Item = &Arc<Provider>> + Send + '_ {
		let explicit_providers = &self.services.config.media_storage_providers;

		let or_all_providers = explicit_providers
			.is_empty()
			.then(|| self.services.storage.providers())
			.into_iter()
			.flatten();

		explicit_providers
			.iter()
			.filter_map(|id| self.services.storage.provider(id).ok())
			.chain(or_all_providers)
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

fn mtime_millis(object: &ObjectMeta) -> u64 {
	u64::try_from(object.last_modified.timestamp_millis()).unwrap_or(0)
}
