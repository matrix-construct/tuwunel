pub mod blurhash;
mod data;
pub(super) mod migrations;
mod preview;
mod remote;
mod retention;
mod tests;
mod thumbnail;
use std::{
	collections::HashSet,
	path::PathBuf,
	sync::Arc,
	time::{Duration, SystemTime},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose};
use ruma::{
	Mxc, OwnedMxcUri, OwnedUserId, UserId, events::GlobalAccountDataEventType,
	http_headers::ContentDisposition,
};
use serde_json::Value;
use tokio::{
	fs,
	io::{AsyncReadExt, AsyncWriteExt, BufReader},
};
use tuwunel_core::{
	Err, Result, debug, debug_error, debug_info, debug_warn, err, error, trace,
	utils::{self, MutexMap},
	warn,
};

pub use self::thumbnail::Dim;
use self::{
	data::{Data, Metadata},
	retention::Retention,
};

#[derive(Debug)]
pub struct FileMeta {
	pub content: Option<Vec<u8>>,
	pub content_type: Option<String>,
	pub content_disposition: Option<ContentDisposition>,
}

pub struct Service {
	url_preview_mutex: MutexMap<String, ()>,
	pub(super) db: Data,
	services: Arc<crate::services::OnceServices>,
	retention: Retention,
}

const MEDIA_RETENTION_ACCOUNT_DATA_KIND: &str = "im.tuwunel.media.retention";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserRetentionPreference {
	Ask,
	Delete,
	Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateAction {
	DeleteImmediately,
	AwaitConfirmation,
	Skip,
}

#[derive(Debug, Clone)]
struct RetentionCandidate {
	mxc: String,
	room_id: Option<String>,
	sender: Option<String>, // user ID who uploaded the media
}

#[derive(Debug, Clone)]
struct CandidateDecision {
	action: CandidateAction,
	owner: Option<OwnedUserId>,
}

/// generated MXC ID (`media-id`) length
pub const MXC_LENGTH: usize = 32;

/// Cache control for immutable objects.
pub const CACHE_CONTROL_IMMUTABLE: &str = "public,max-age=31536000,immutable";

/// Default cross-origin resource policy.
pub const CORP_CROSS_ORIGIN: &str = "cross-origin";

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			url_preview_mutex: MutexMap::new(),
			db: Data::new(args.db),
			services: args.services.clone(),
			retention: Retention::new(args.db),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		self.create_media_dir().await?;

		// startup summary for retention configuration
		warn!(
			policy = self
				.services
				.server
				.config
				.media_retention_on_redaction(),
			grace = self
				.services
				.server
				.config
				.media_retention_grace_period_secs(),
			"retention: startup configuration"
		);

		// deletion worker loop (scaffold): runs periodically respecting grace period
		let grace = Duration::from_secs(
			self.services
				.server
				.config
				.media_retention_grace_period_secs(),
		);
		let retention = self.retention.clone();
		let this = self.clone();
		warn!("creating media deletion worker");
		tokio::spawn(async move {
			loop {
				if let Err(e) = retention.worker_process_queue(&this, grace).await {
					debug_warn!("media retention worker error: {e}");
				}
				tokio::time::sleep(Duration::from_secs(10)).await;
			}
		});

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	// below helpers can be called by message processing pipelines when events are
	// created/edited/redacted.
	pub fn retention_insert_mxcs_on_event(
		&self,
		event_id: &str,
		room_id: &str,
		sender: &str,
		mxcs: &[(String, bool, String)],
	) {
		self.retention
			.insert_mxcs_on_event(event_id, room_id, sender, mxcs);
	}

	/// Track a media upload for potential association with an upcoming encrypted event.
	pub fn retention_track_pending_upload(&self, user_id: &str, mxc: &str) {
		self.retention.track_pending_upload(user_id, mxc);
	}

	/// Consume pending uploads for a user and return them as MXC refs for an encrypted event.
	pub async fn retention_consume_pending_uploads(
		&self,
		user_id: &str,
		event_ts: u64,
	) -> Vec<(String, bool, String)> {
		self.retention.consume_pending_uploads(user_id, event_ts).await
	}

	pub async fn retention_decrement_on_redaction(&self, event_id: &str) {
		use self::retention::RetentionPolicy;

		let policy = RetentionPolicy::from_str(
			self.services
				.server
				.config
				.media_retention_on_redaction(),
		);
		let mut candidates: Vec<RetentionCandidate> = Vec::new();
		let mut event_value: Option<Value> = None;

		if let Ok(primary) = self
			.retention
			.decrement_refcount_on_redaction(event_id, policy)
			.await
		{
			if !primary.is_empty() {
				candidates.extend(
					primary
						.into_iter()
						.map(|(mxc, room_id, sender)| RetentionCandidate { mxc, room_id: Some(room_id), sender }),
				);
			}
		}

		if let Ok(parsed_eid) = ruma::EventId::parse(event_id) {
			match self
				.services
				.timeline
				.get_pdu_json(&parsed_eid)
				.await
			{
				| Ok(canonical) => match serde_json::to_value(&canonical) {
					| Ok(val) => {
						if candidates.is_empty() {
							let mut discovered = HashSet::new();
							collect_mxcs(&val, &mut discovered);
							if !discovered.is_empty() {
								let room_id = val
									.get("room_id")
									.and_then(|v| v.as_str())
									.map(str::to_owned);
								candidates.extend(discovered.into_iter().map(|mxc| {
									RetentionCandidate { mxc, room_id: room_id.clone(), sender: None }
								}));
							}
						}
						event_value = Some(val);
					},
					| Err(e) => {
						warn!(%event_id, "retention: failed to convert canonical event to json value: {e}")
					},
				},
				| Err(e) => {
					debug_warn!(%event_id, "retention: unable to load original event for redaction: {e}")
				},
			}
		}

		if candidates.is_empty() {
			debug!(%event_id, "retention: no media discovered for redaction");
			return;
		}

		for candidate in candidates {
			// Evaluate candidate using policy and user preferences
			let decision = self
				.evaluate_retention_candidate(policy, event_value.as_ref(), &candidate)
				.await;

			match (decision.action, decision.owner) {
				| (CandidateAction::DeleteImmediately, owner) => {
					self.retention.queue_media_for_deletion(
						&candidate.mxc,
						owner.as_deref(),
						false,
					);
				},
				| (CandidateAction::AwaitConfirmation, Some(owner)) => {
					self.retention.queue_media_for_deletion(
						&candidate.mxc,
						Some(owner.as_ref()),
						true,
					);

					// Send notification to the uploader's user room (not the room where it was posted!)
					if self.services.globals.user_is_local(owner.as_ref()) {
						let body = self.build_retention_notice(&candidate, event_value.as_ref());
						if let Err(e) = self
							.services
							.userroom
							.send_text(owner.as_ref(), &body)
							.await
						{
							warn!(
								%event_id,
								mxc = %candidate.mxc,
								user = owner.as_str(),
								"retention: failed to notify user about pending deletion: {e}",
							);
						} else {
							debug_info!(
								%event_id,
								mxc = %candidate.mxc,
								user = owner.as_str(),
								"retention: sent user confirmation request to their user room"
							);
						}
					}
				},
				| (CandidateAction::AwaitConfirmation, None) => {
					warn!(%event_id, mxc = %candidate.mxc, "retention: confirmation requested but owner is unknown");
				},
				| (CandidateAction::Skip, _) => {
					debug!(%event_id, mxc = %candidate.mxc, "retention: skipping deletion for candidate");
				},
			}
		}
	}

	async fn evaluate_retention_candidate(
		&self,
		policy: retention::RetentionPolicy,
		event_value: Option<&Value>,
		candidate: &RetentionCandidate,
	) -> CandidateDecision {
		use self::retention::RetentionPolicy;

		if matches!(policy, RetentionPolicy::Keep) {
			return CandidateDecision {
				action: CandidateAction::Skip,
				owner: None,
			};
		}

		// Prefer sender from candidate (who uploaded the media) over database lookup
		let mut owner: Option<OwnedUserId> = candidate
			.sender
			.as_deref()
			.and_then(|s| OwnedUserId::try_from(s.to_owned()).ok());
		
		// Fallback to database lookup if sender not in candidate
		if owner.is_none() {
			owner = self.db.get_media_owner(&candidate.mxc).await;
		}
		
		// Last resort: try to get from event value
		if owner.is_none() {
			if let Some(val) = event_value {
				if let Some(sender) = val.get("sender").and_then(|s| s.as_str()) {
					if let Ok(parsed) = OwnedUserId::try_from(sender.to_owned()) {
						owner = Some(parsed);
					}
				}
			}
		}

		if let Some(owner_id) = owner.as_ref() {
			if !self
				.services
				.globals
				.user_is_local(owner_id.as_ref())
			{
				let action = match policy {
					| RetentionPolicy::Keep => CandidateAction::Skip,
					| RetentionPolicy::DeleteIfUnreferenced
					| RetentionPolicy::ForceDeleteLocal => CandidateAction::DeleteImmediately,
				};
				return CandidateDecision { action, owner };
			}

			match self
				.user_retention_preference(owner_id.as_ref())
				.await
			{
				| UserRetentionPreference::Delete => CandidateDecision {
					action: CandidateAction::DeleteImmediately,
					owner,
				},
				| UserRetentionPreference::Keep =>
					CandidateDecision { action: CandidateAction::Skip, owner },
				| UserRetentionPreference::Ask => CandidateDecision {
					action: CandidateAction::AwaitConfirmation,
					owner,
				},
			}
		} else {
			let action = match policy {
				| RetentionPolicy::Keep => CandidateAction::Skip,
				| RetentionPolicy::DeleteIfUnreferenced | RetentionPolicy::ForceDeleteLocal =>
					CandidateAction::DeleteImmediately,
			};
			CandidateDecision { action, owner: None }
		}
	}

	fn build_retention_notice(
		&self,
		candidate: &RetentionCandidate,
		event_value: Option<&Value>,
	) -> String {
		let room_segment = candidate
			.room_id
			.as_deref()
			.map(|room| format!(" in room {room}"))
			.unwrap_or_default();

		let timestamp = event_value
			.and_then(|val| val.get("origin_server_ts"))
			.and_then(canonical_json_to_u64)
			.map(|ts| format!(" at {ts}"))
			.unwrap_or_default();

		format!(
			"A piece of media ({mxc}) you uploaded{room_segment}{timestamp} is pending deletion. \
			Run `!user retention confirm {mxc}` here to delete it now, or update your media retention preference to keep it.",
			mxc = candidate.mxc
		)
	}

	pub async fn retention_confirm_deletion(&self, user: &UserId, mxc: &str) -> Result<u64> {
		self.retention.confirm_candidate(self, mxc, user).await
	}

	async fn user_retention_preference(&self, user: &UserId) -> UserRetentionPreference {
		if !self.services.globals.user_is_local(user) {
			return UserRetentionPreference::Delete;
		}

		let kind = GlobalAccountDataEventType::from(MEDIA_RETENTION_ACCOUNT_DATA_KIND);
		match self
			.services
			.account_data
			.get_global::<Value>(user, kind)
			.await
		{
			| Ok(value) =>
				parse_user_retention_preference(&value).unwrap_or(UserRetentionPreference::Ask),
			| Err(e) => {
				debug!(user = user.as_str(), "retention: failed to load user preference: {e}");
				UserRetentionPreference::Ask
			},
		}
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
		let mut f = self.create_media_file(&key).await?;
		f.write_all(file).await?;

		Ok(())
	}

	/// Deletes a file in the database and from the media directory via an MXC
	pub async fn delete(&self, mxc: &Mxc<'_>) -> Result {
		match self.db.search_mxc_metadata_prefix(mxc).await {
			| Ok(keys) => {
				for key in keys {
					trace!(?mxc, "MXC Key: {key:?}");
					debug_info!(?mxc, "Deleting from filesystem");

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
	pub async fn delete_from_user(&self, user: &UserId) -> Result<usize> {
		let mxcs = self.db.get_all_user_mxcs(user).await;
		let mut deletion_count: usize = 0;

		for mxc in mxcs {
			let Ok(mxc) = mxc.as_str().try_into().inspect_err(|e| {
				debug_error!(?mxc, "Failed to parse MXC URI from database: {e}");
			}) else {
				continue;
			};

			debug_info!(%deletion_count, "Deleting MXC {mxc} by user {user} from database and filesystem");
			match self.delete(&mxc).await {
				| Ok(()) => {
					deletion_count = deletion_count.saturating_add(1);
				},
				| Err(e) => {
					debug_error!(%deletion_count, "Failed to delete {mxc} from user {user}, ignoring error: {e}");
				},
			}
		}

		Ok(deletion_count)
	}

	/// Downloads a file.
	pub async fn get(&self, mxc: &Mxc<'_>) -> Result<Option<FileMeta>> {
		match self
			.db
			.search_file_metadata(mxc, &Dim::default())
			.await
		{
			| Ok(Metadata { content_disposition, content_type, key }) => {
				let mut content = Vec::with_capacity(8192);
				let path = self.get_media_file(&key);
				BufReader::new(fs::File::open(path).await?)
					.read_to_end(&mut content)
					.await?;

				Ok(Some(FileMeta {
					content: Some(content),
					content_type,
					content_disposition,
				}))
			},
			| _ => Ok(None),
		}
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

	/// Deletes all remote only media files in the given at or after
	/// time/duration. Returns a usize with the amount of media files deleted.
	pub async fn delete_all_remote_media_at_after_time(
		&self,
		time: SystemTime,
		before: bool,
		after: bool,
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

			let path = self.get_media_file(&key);

			let file_metadata = match fs::metadata(path.clone()).await {
				| Ok(file_metadata) => file_metadata,
				| Err(e) => {
					error!(
						"Failed to obtain file metadata for MXC {mxc} at file path \
						 \"{path:?}\", skipping: {e}"
					);
					continue;
				},
			};

			trace!(%mxc, ?path, "File metadata: {file_metadata:?}");

			let file_created_at = match file_metadata.created() {
				| Ok(value) => value,
				| Err(err) if err.kind() == std::io::ErrorKind::Unsupported => {
					debug!("btime is unsupported, using mtime instead");
					file_metadata.modified()?
				},
				| Err(err) => {
					error!("Could not delete MXC {mxc} at path {path:?}: {err:?}. Skipping...");
					continue;
				},
			};

			debug!("File created at: {file_created_at:?}");

			if file_created_at >= time && before {
				debug!(
					"File is within (before) user duration, pushing to list of file paths and \
					 keys to delete."
				);
				remote_mxcs.push(mxc.to_string());
			} else if file_created_at <= time && after {
				debug!(
					"File is not within (after) user duration, pushing to list of file paths \
					 and keys to delete."
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
		let path = self.get_media_file(key);
		let legacy = self.get_media_file_b64(key);
		debug!(?key, ?path, ?legacy, "Removing media file");

		let file_rm = fs::remove_file(&path);
		let legacy_rm = fs::remove_file(&legacy);
		let (file_rm, legacy_rm) = tokio::join!(file_rm, legacy_rm);
		if let Err(e) = legacy_rm {
			if self.services.server.config.media_compat_file_link {
				debug_error!(?key, ?legacy, "Failed to remove legacy media symlink: {e}");
			}
		}

		Ok(file_rm?)
	}

	async fn create_media_file(&self, key: &[u8]) -> Result<fs::File> {
		let path = self.get_media_file(key);
		debug!(?key, ?path, "Creating media file");

		let file = fs::File::create(&path).await?;
		if self.services.server.config.media_compat_file_link {
			let legacy = self.get_media_file_b64(key);
			if let Err(e) = fs::symlink(&path, &legacy).await {
				debug_error!(
					key = ?encode_key(key), ?path, ?legacy,
					"Failed to create legacy media symlink: {e}"
				);
			}
		}

		Ok(file)
	}

	#[inline]
	pub async fn get_metadata(&self, mxc: &Mxc<'_>) -> Option<FileMeta> {
		self.db
			.search_file_metadata(mxc, &Dim::default())
			.await
			.map(|metadata| FileMeta {
				content_disposition: metadata.content_disposition,
				content_type: metadata.content_type,
				content: None,
			})
			.ok()
	}

	#[inline]
	#[must_use]
	pub fn get_media_file(&self, key: &[u8]) -> PathBuf { self.get_media_file_sha256(key) }

	/// new SHA256 file name media function. requires database migrated. uses
	/// SHA256 hash of the base64 key as the file name
	#[must_use]
	pub fn get_media_file_sha256(&self, key: &[u8]) -> PathBuf {
		let mut r = self.get_media_dir();
		// Using the hash of the base64 key as the filename
		// This is to prevent the total length of the path from exceeding the maximum
		// length in most filesystems
		let digest = <sha2::Sha256 as sha2::Digest>::digest(key);
		let encoded = encode_key(&digest);
		r.push(encoded);
		r
	}

	/// old base64 file name media function
	/// This is the old version of `get_media_file` that uses the full base64
	/// key as the filename.
	#[must_use]
	pub fn get_media_file_b64(&self, key: &[u8]) -> PathBuf {
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

fn parse_user_retention_preference(value: &Value) -> Option<UserRetentionPreference> {
	if let Some(mode) = value.get("mode").and_then(|v| v.as_str()) {
		return match mode {
			| "delete" | "auto" => Some(UserRetentionPreference::Delete),
			| "keep" => Some(UserRetentionPreference::Keep),
			| "ask" => Some(UserRetentionPreference::Ask),
			| _ => None,
		};
	}

	if let Some(confirm) = value
		.get("confirm_before_delete")
		.and_then(|v| v.as_bool())
	{
		return Some(if confirm {
			UserRetentionPreference::Ask
		} else {
			UserRetentionPreference::Delete
		});
	}

	if let Some(keep) = value.get("retain").and_then(|v| v.as_bool()) {
		return Some(if keep {
			UserRetentionPreference::Keep
		} else {
			UserRetentionPreference::Delete
		});
	}

	None
}

fn collect_mxcs(value: &Value, out: &mut HashSet<String>) {
	match value {
		| Value::String(s) if s.starts_with("mxc://") => {
			out.insert(s.to_owned());
		},
		| Value::Array(arr) =>
			for item in arr {
				collect_mxcs(item, out);
			},
		| Value::Object(map) =>
			for item in map.values() {
				collect_mxcs(item, out);
			},
		| _ => {},
	}
}

fn canonical_json_to_u64(value: &Value) -> Option<u64> {
	match value {
		| Value::Number(num) => num.as_u64(),
		| Value::String(s) => s.parse::<u64>().ok(),
		| _ => None,
	}
}

#[inline]
#[must_use]
pub fn encode_key(key: &[u8]) -> String { general_purpose::URL_SAFE_NO_PAD.encode(key) }
