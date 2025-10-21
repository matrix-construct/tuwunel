use std::{
	path::PathBuf,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use futures::StreamExt;
use ruma::UserId;
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, err, trace, warn};
use tuwunel_database::{Cbor, Deserialized, Map, keyval::serialize_val};

use super::Service;

//todo: split into multiple files

/// keyspace prefixes inside the `media_retention` CF
const K_MREF: &str = "mref:"; // mref:<mxc>
const K_MER: &str = "mer:"; // mer:<event_id>:<kind>
const K_QUEUE: &str = "qdel:"; // qdel:<mxc> => DeletionCandidate
const K_PENDING: &str = "pending:"; // pending:<user_id>:<timestamp_ms> => PendingUpload
const K_PREFS: &str = "prefs:"; // prefs:<user_id> => UserRetentionPrefs

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct UserRetentionPrefs {
	/// Auto-delete media in unencrypted rooms without asking
	#[serde(default)]
	pub auto_delete_unencrypted: bool,
	/// Auto-delete media in encrypted rooms without asking
	/// Warning: Detection is based on pending uploads, may have false positives
	#[serde(default)]
	pub auto_delete_encrypted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PendingUpload {
	pub mxc: String,
	pub user_id: String,
	pub upload_ts: u64, // milliseconds since epoch
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct MediaRef {
	pub refcount: i64,
	pub local: bool,
	pub first_seen_ts: u64,
	pub last_seen_ts: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct MediaEventRef {
	pub mxc: String,
	pub room_id: String,
	pub kind: String, // "content.url", "thumbnail_url"
	#[serde(default)]
	pub sender: Option<String>, // user ID who uploaded/sent this media
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct DeletionCandidate {
	pub mxc: String,
	pub enqueued_ts: u64,
	#[serde(default)]
	pub user_id: Option<String>,
	#[serde(default)]
	pub awaiting_confirmation: bool,
	/// Event ID of the notification message sent to the user (for reaction
	/// handling)
	#[serde(default)]
	pub notification_event_id: Option<String>,
	/// Event ID of the ✅ reaction (for cleanup)
	#[serde(default)]
	pub confirm_reaction_id: Option<String>,
	/// Event ID of the ❌ reaction (for cleanup)
	#[serde(default)]
	pub cancel_reaction_id: Option<String>,
	/// Event ID of the ♻️ reaction (always auto-delete for this room type)
	#[serde(default)]
	pub auto_reaction_id: Option<String>,
	/// Was this media detected as being from an encrypted room?
	/// (based on pending upload matching, may have false positives)
	#[serde(default)]
	pub from_encrypted_room: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetentionPolicy {
	Keep,
	AskSender,
	DeleteAlways,
}

impl RetentionPolicy {
	pub(super) fn from_str(s: &str) -> Self {
		match s {
			| "ask_sender" => Self::AskSender,
			| "delete_always" => Self::DeleteAlways,
			| _ => Self::Keep,
		}
	}
}

#[derive(Clone)]
pub struct Retention {
	cf: Arc<Map>,
}

impl Retention {
	pub(super) fn new(db: &Arc<tuwunel_database::Database>) -> Self {
		Self { cf: db["media_retention"].clone() }
	}

	#[inline]
	fn key_mref(mxc: &str) -> String { format!("{K_MREF}{mxc}") }

	#[inline]
	fn key_mer(event_id: &str, kind: &str) -> String { format!("{K_MER}{event_id}:{kind}") }

	#[inline]
	fn key_queue(mxc: &str) -> String { format!("{K_QUEUE}{mxc}") }

	#[inline]
	fn key_pending(user_id: &str, timestamp_ms: u64) -> String {
		format!("{K_PENDING}{user_id}:{timestamp_ms}")
	}

	#[inline]
	fn pending_prefix(user_id: &str) -> String { format!("{K_PENDING}{user_id}:") }

	#[inline]
	fn key_prefs(user_id: &str) -> String { format!("{K_PREFS}{user_id}") }

	/// Get user's retention preferences
	pub async fn get_user_prefs(&self, user_id: &str) -> UserRetentionPrefs {
		let key = Self::key_prefs(user_id);
		match self.cf.get(&key).await {
			| Ok(handle) => match handle.deserialized::<Cbor<UserRetentionPrefs>>() {
				| Ok(Cbor(prefs)) => prefs,
				| Err(e) => {
					warn!(%user_id, "retention: failed to deserialize user prefs: {e}");
					UserRetentionPrefs::default()
				},
			},
			| Err(_) => UserRetentionPrefs::default(),
		}
	}

	/// Save user's retention preferences
	pub async fn set_user_prefs(&self, user_id: &str, prefs: &UserRetentionPrefs) -> Result<()> {
		let key = Self::key_prefs(user_id);
		self.cf.raw_put(&key, Cbor(prefs));
		Ok(())
	}

	#[allow(dead_code)]
	pub(super) async fn get_media_ref(&self, mxc: &str) -> Result<Option<MediaRef>> {
		match self.cf.get(&Self::key_mref(mxc)).await {
			| Ok(handle) => Ok(Some(handle.deserialized::<Cbor<_>>()?.0)),
			| Err(_) => Ok(None),
		}
	}

	#[allow(dead_code)]
	pub(super) fn put_media_ref(&self, mxc: &str, mr: &MediaRef) {
		self.cf.raw_put(Self::key_mref(mxc), Cbor(mr));
	}

	#[allow(dead_code)]
	pub(super) async fn get_media_event_ref(
		&self,
		event_id: &str,
		kind: &str,
	) -> Result<Option<MediaEventRef>> {
		match self.cf.get(&Self::key_mer(event_id, kind)).await {
			| Ok(handle) => Ok(Some(handle.deserialized::<Cbor<_>>()?.0)),
			| Err(_) => Ok(None),
		}
	}

	#[allow(dead_code)]
	pub(super) fn put_media_event_ref(&self, event_id: &str, mer: &MediaEventRef) {
		let key = Self::key_mer(event_id, &mer.kind);
		self.cf.raw_put(key, Cbor(mer));
	}

	/// insert/update references for a newly created or edited event.
	///
	/// assumptions:
	/// - `mxcs` is a slice of (mxc_uri, local, kind)
	/// - `sender` is the user ID who sent/uploaded this media
	pub(super) fn insert_mxcs_on_event(
		&self,
		event_id: &str,
		room_id: &str,
		sender: &str,
		mxcs: &[(String, bool, String)],
	) {
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_secs();
		if mxcs.is_empty() {
			warn!(%event_id, "retention: insert called with zero MXCs");
			return;
		}
		warn!(%event_id, count = mxcs.len(), %room_id, sender=%sender, "retention: inserting media refs for event");

		let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(mxcs.len().saturating_mul(2));
		for (mxc, local, kind) in mxcs {
			// update MediaEventRef
			let mer = MediaEventRef {
				mxc: mxc.clone(),
				room_id: room_id.to_owned(),
				kind: kind.clone(),
				sender: Some(sender.to_owned()),
			};
			let key_mer = Self::key_mer(event_id, kind).into_bytes();
			let val_mer = serialize_val(Cbor(&mer))
				.expect("serialize mer")
				.to_vec();
			puts.push((key_mer, val_mer));

			// upsert MediaRef
			let key_mref = Self::key_mref(mxc);
			let current = self.cf.get_blocking(&key_mref);
			let (mr, new) = match current.and_then(|h| h.deserialized::<Cbor<MediaRef>>()) {
				| Ok(Cbor(mut v)) => {
					v.refcount = v.refcount.saturating_add(1);
					v.last_seen_ts = now;
					(v, false)
				},
				| _ => (
					MediaRef {
						refcount: 1,
						local: *local,
						first_seen_ts: now,
						last_seen_ts: now,
					},
					true,
				),
			};
			if new {
				warn!(%event_id, %mxc, %kind, local = local, refcount = mr.refcount, "retention: new media ref");
			} else {
				warn!(%event_id, %mxc, %kind, local = local, refcount = mr.refcount, "retention: increment media ref");
			}
			let val_mref = serialize_val(Cbor(&mr))
				.expect("serialize mref")
				.to_vec();
			puts.push((key_mref.into_bytes(), val_mref));
		}
		self.cf.write_batch_raw(puts, std::iter::empty());
	}

	/// decrement refcounts for all MediaEventRef mapped by this event id.
	/// if policy is set to delete unreferenced/local, enqueue for deletion
	/// Returns Vec<(mxc, room_id, sender)>
	pub(super) async fn decrement_refcount_on_redaction(
		&self,
		event_id: &str,
		policy: RetentionPolicy,
	) -> Result<Vec<(String, String, Option<String>)>> {
		warn!(%event_id, ?policy, "retention: redaction decrement start");
		let prefix = format!("{K_MER}{event_id}:");
		let prefixb = prefix.as_bytes().to_vec();
		let mut to_delete: Vec<(String, String, Option<String>)> = Vec::new();
		let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
		let mut dels: Vec<Vec<u8>> = Vec::new();
		let mut processed = 0_usize;

		let mut stream = self
			.cf
			.stream_raw_prefix::<&str, Cbor<MediaEventRef>, _>(&prefixb);
		while let Some(item) = stream.next().await.transpose()? {
			let (key, Cbor(mer)) = item;
			processed = processed.saturating_add(1);
			// load MediaRef
			let key_mref = Self::key_mref(&mer.mxc);
			let current = self.cf.get(&key_mref).await.ok();
			if let Some(handle) = current {
				let Cbor(mut mr): Cbor<MediaRef> = handle.deserialized::<Cbor<MediaRef>>()?;
				mr.refcount = mr.refcount.saturating_sub(1);
				mr.last_seen_ts = now_secs();
				let should_queue = match policy {
					| RetentionPolicy::Keep => false,
					| RetentionPolicy::AskSender => mr.refcount == 0,
					| RetentionPolicy::DeleteAlways => mr.local,
				};
				warn!(%event_id, mxc = %mer.mxc, kind = %mer.kind, new_refcount = mr.refcount, should_queue, local = mr.local, sender = ?mer.sender, "retention: redaction updated ref");
				let val_mref = serialize_val(Cbor(&mr))?.to_vec();
				puts.push((key_mref.into_bytes(), val_mref));
				if should_queue {
					warn!(%event_id, mxc = %mer.mxc, room = %mer.room_id, sender = ?mer.sender, "retention: media candidate ready for deletion");
					to_delete.push((mer.mxc.clone(), mer.room_id.clone(), mer.sender.clone()));
				}
			}

			// remove the mer entry regardless
			dels.push(key.as_bytes().to_vec());
		}
		self.cf.write_batch_raw(puts, dels);
		if processed == 0 {
			warn!(%event_id, "retention: no media event refs found on redaction; did insert run during creation?");
		}
		warn!(%event_id, queued = to_delete.len(), processed, "retention: redaction decrement complete");
		Ok(to_delete)
	}

	/// Track a media upload that might be used in an upcoming encrypted
	/// message. These pending uploads will be matched to encrypted events
	/// within a time window.
	pub(super) fn track_pending_upload(&self, user_id: &str, mxc: &str) {
		let upload_ts = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis()
			.try_into()
			.unwrap_or_default();

		let pending = PendingUpload {
			mxc: mxc.to_owned(),
			user_id: user_id.to_owned(),
			upload_ts,
		};

		let key = Self::key_pending(user_id, upload_ts);
		warn!(
			user_id,
			mxc, upload_ts, "retention: tracking pending upload for encrypted event association"
		);
		self.cf.raw_put(key, Cbor(&pending));

		// Clean up old pending uploads (older than 60 seconds) asynchronously
		self.cleanup_old_pending_uploads(user_id, upload_ts);
	}

	/// Find and consume pending uploads for a user within the last N seconds.
	/// Returns Vec<(mxc, local, kind)> suitable for insert_mxcs_on_event.
	/// Time window: 60 seconds (uploads must have happened within last minute).
	pub(super) async fn consume_pending_uploads(
		&self,
		user_id: &str,
		event_ts: u64, // event timestamp in milliseconds
	) -> Vec<(String, bool, String)> {
		let window_ms = 60_000_u64; // 60 seconds
		let cutoff_ts = event_ts.saturating_sub(window_ms);

		let prefix = Self::pending_prefix(user_id);
		let mut found_mxcs: Vec<(String, bool, String)> = Vec::new();
		let mut to_delete: Vec<Vec<u8>> = Vec::new();

		let mut stream = self
			.cf
			.stream_raw_prefix::<&str, Cbor<PendingUpload>, _>(prefix.as_bytes());

		while let Some(item) = stream.next().await.transpose().ok().flatten() {
			let (key, Cbor(pending)) = item;

			// Only match uploads within the time window
			if pending.upload_ts >= cutoff_ts && pending.upload_ts <= event_ts {
				// Assume local=true since user uploaded to our server
				// Mark as encrypted media
				found_mxcs.push((pending.mxc.clone(), true, "encrypted.media".to_owned()));
				to_delete.push(key.as_bytes().to_vec());
				warn!(
					user_id,
					mxc = %pending.mxc,
					upload_ts = pending.upload_ts,
					event_ts,
					"retention: consuming pending upload for encrypted event"
				);
			} else if pending.upload_ts < cutoff_ts {
				// Too old, clean it up
				to_delete.push(key.as_bytes().to_vec());
			}
		}

		// Remove consumed/old pending uploads
		if !to_delete.is_empty() {
			self.cf
				.write_batch_raw(std::iter::empty(), to_delete);
		}

		found_mxcs
	}

	/// Clean up pending uploads older than 60 seconds for a specific user.
	fn cleanup_old_pending_uploads(&self, user_id: &str, current_ts: u64) {
		let cf = self.cf.clone();
		let user_id = user_id.to_owned();
		let cutoff = current_ts.saturating_sub(60_000);

		// Spawn cleanup task to avoid blocking
		tokio::spawn(async move {
			let prefix = Self::pending_prefix(&user_id);
			let mut to_delete: Vec<Vec<u8>> = Vec::new();

			let mut stream =
				cf.stream_raw_prefix::<&str, Cbor<PendingUpload>, _>(prefix.as_bytes());

			while let Some(item) = stream.next().await.transpose().ok().flatten() {
				let (key, Cbor(pending)) = item;
				if pending.upload_ts < cutoff {
					to_delete.push(key.as_bytes().to_vec());
				}
			}

			if !to_delete.is_empty() {
				let count = to_delete.len();
				cf.write_batch_raw(std::iter::empty(), to_delete);
				trace!(user_id, count, "retention: cleaned up old pending uploads");
			}
		});
	}

	/// queue a media item for deletion (idempotent best-effort).
	pub(super) fn queue_media_for_deletion(
		&self,
		mxc: &str,
		owner: Option<&UserId>,
		awaiting_confirmation: bool,
		notification_event_id: Option<String>,
		confirm_reaction_id: Option<String>,
		cancel_reaction_id: Option<String>,
		auto_reaction_id: Option<String>,
		from_encrypted_room: bool,
	) {
		let key = Self::key_queue(mxc);
		// overwrite / insert candidate with fresh timestamp
		let cand = DeletionCandidate {
			mxc: mxc.to_owned(),
			enqueued_ts: now_secs(),
			user_id: owner.map(ToString::to_string),
			awaiting_confirmation,
			notification_event_id,
			confirm_reaction_id,
			cancel_reaction_id,
			auto_reaction_id,
			from_encrypted_room,
		};
		warn!(
			mxc,
			awaiting_confirmation,
			owner = owner.map(UserId::as_str),
			from_encrypted = from_encrypted_room,
			"retention: queue media for deletion (awaiting user confirmation)"
		);
		self.cf.raw_put(key, Cbor(&cand));
	}

	/// Delete media immediately (for auto-delete and "delete_always" mode)
	/// event-driven
	pub(super) async fn delete_media_immediately(
		&self,
		service: &Service,
		mxc: &str,
		owner: Option<&UserId>,
		from_encrypted_room: bool,
	) -> Result<u64> {
		let deleted_bytes = self.delete_local_media(service, mxc).await?;

		// Remove metadata entries
		let dels = vec![Self::key_mref(mxc).into_bytes()];
		self.cf.write_batch_raw(std::iter::empty(), dels);

		warn!(
			mxc,
			bytes = deleted_bytes,
			owner = owner.map(UserId::as_str),
			from_encrypted = from_encrypted_room,
			"retention: media deleted immediately (event-driven)"
		);

		Ok(deleted_bytes)
	}

	pub(super) async fn confirm_candidate(
		&self,
		service: &Service,
		mxc: &str,
		requester: &UserId,
	) -> Result<(u64, Option<String>)> {
		let key = Self::key_queue(mxc);
		let handle = self
			.cf
			.get(&key)
			.await
			.map_err(|_| err!(Request(NotFound("no pending deletion for this media"))))?;
		let Cbor(mut candidate) = handle.deserialized::<Cbor<DeletionCandidate>>()?;

		let Some(owner) = candidate.user_id.as_deref() else {
			return Err(err!(Request(Forbidden("media candidate owner unknown"))));
		};
		if owner != requester.as_str() {
			return Err(err!(Request(Forbidden("media candidate owned by another user"))));
		}
		if !candidate.awaiting_confirmation {
			return Err(err!(Request(InvalidParam("media deletion already processed",))));
		}

		// Save the cancel reaction ID to redact it
		let cancel_reaction_to_redact = candidate.cancel_reaction_id.clone();

		candidate.awaiting_confirmation = false;
		candidate.enqueued_ts = now_secs();

		let deleted_bytes = self.delete_local_media(service, mxc).await?;
		let dels = vec![key.into_bytes(), Self::key_mref(mxc).into_bytes()];
		self.cf.write_batch_raw(std::iter::empty(), dels);
		warn!(
			mxc,
			bytes = deleted_bytes,
			user = requester.as_str(),
			"retention: media deletion confirmed by user"
		);
		Ok((deleted_bytes, cancel_reaction_to_redact))
	}

	/// Find MXC by notification event ID (for reaction-based confirmation)
	pub(super) async fn find_mxc_by_notification_event(
		&self,
		notification_event_id: &str,
	) -> Option<String> {
		let prefix = K_QUEUE.as_bytes();
		let mut stream = self
			.cf
			.stream_raw_prefix::<&str, Cbor<DeletionCandidate>, _>(&prefix);

		while let Some(item) = stream.next().await.transpose().ok().flatten() {
			let (_key, Cbor(cand)) = item;
			if let Some(ref stored_event_id) = cand.notification_event_id {
				if stored_event_id == notification_event_id {
					return Some(cand.mxc.clone());
				}
			}
		}

		None
	}

	/// Cancel a deletion candidate (remove from queue)
	/// Returns the confirm reaction ID to redact it
	pub(super) async fn cancel_candidate(
		&self,
		mxc: &str,
		requester: &UserId,
	) -> Result<Option<String>> {
		let key = Self::key_queue(mxc);
		match self.cf.get(&key).await {
			| Ok(handle) => {
				let Cbor(candidate) = handle.deserialized::<Cbor<DeletionCandidate>>()?;

				let Some(owner) = candidate.user_id.as_deref() else {
					return Err(err!(Request(Forbidden("media candidate owner unknown"))));
				};
				if owner != requester.as_str() {
					return Err(err!(Request(Forbidden(
						"media candidate owned by another user"
					))));
				}

				// Save the confirm reaction ID to redact it
				let confirm_reaction_to_redact = candidate.confirm_reaction_id.clone();

				// Remove from queue
				self.cf.remove(key.as_str());
				warn!(
					mxc,
					user = requester.as_str(),
					"retention: media deletion cancelled by user"
				);
				Ok(confirm_reaction_to_redact)
			},
			| Err(_) => Err(err!(Request(NotFound("no pending deletion for this media")))),
		}
	}

	/// Enable auto-delete for the room type (encrypted/unencrypted) and confirm
	/// deletion Returns: (deleted_bytes, confirm_reaction_id,
	/// cancel_reaction_id) to redact unused reactions
	pub(super) async fn auto_delete_candidate(
		&self,
		service: &Service,
		mxc: &str,
		requester: &UserId,
	) -> Result<(u64, Option<String>, Option<String>, bool)> {
		let key = Self::key_queue(mxc);
		match self.cf.get(&key).await {
			| Ok(handle) => {
				let Cbor(candidate) = handle.deserialized::<Cbor<DeletionCandidate>>()?;

				let Some(owner) = candidate.user_id.as_deref() else {
					return Err(err!(Request(Forbidden("media candidate owner unknown"))));
				};
				if owner != requester.as_str() {
					return Err(err!(Request(Forbidden(
						"media candidate owned by another user"
					))));
				}

				let from_encrypted_room = candidate.from_encrypted_room;

				let mut prefs = self.get_user_prefs(requester.as_str()).await;
				if from_encrypted_room {
					prefs.auto_delete_encrypted = true;
					warn!(user = %requester, "retention: enabled auto-delete for encrypted rooms");
				} else {
					prefs.auto_delete_unencrypted = true;
					warn!(user = %requester, "retention: enabled auto-delete for unencrypted rooms");
				}
				self.set_user_prefs(requester.as_str(), &prefs)
					.await?;

				let confirm_reaction_to_redact = candidate.confirm_reaction_id.clone();
				let cancel_reaction_to_redact = candidate.cancel_reaction_id.clone();

				let deleted_bytes = self.delete_local_media(service, mxc).await?;
				let dels = vec![key.into_bytes(), Self::key_mref(mxc).into_bytes()];
				self.cf.write_batch_raw(std::iter::empty(), dels);
				warn!(
					mxc,
					bytes = deleted_bytes,
					user = requester.as_str(),
					from_encrypted = from_encrypted_room,
					"retention: media auto-deleted and preference saved"
				);
				Ok((
					deleted_bytes,
					confirm_reaction_to_redact,
					cancel_reaction_to_redact,
					from_encrypted_room,
				))
			},
			| Err(_) => Err(err!(Request(NotFound("no pending deletion for this media")))),
		}
	}

	async fn delete_local_media(&self, service: &Service, mxc: &str) -> Result<u64> {
		// delete original + thumbnails (any dimensions)
		use ruma::Mxc;
		let mxc_parsed: Mxc<'_> = mxc
			.try_into()
			.map_err(|_| err!(Request(BadJson("invalid mxc"))))?;

		// delete originals
		let keys = service
			.db
			.search_mxc_metadata_prefix(&mxc_parsed)
			.await
			.unwrap_or_default();
		let mut total = 0_u64;
		for key in keys {
			let path = service.get_media_file(&key);
			total = total.saturating_add(remove_file_tolerant(&path));
			let legacy = service.get_media_file_b64(&key);
			total = total.saturating_add(remove_file_tolerant(&legacy));
		}
		warn!("retention: total bytes deleted {total}");
		Ok(total)
	}
}

fn now_secs() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_secs()
}

fn remove_file_tolerant(path: &PathBuf) -> u64 {
	match std::fs::metadata(path) {
		| Ok(meta) => {
			let len = meta.len();
			if let Err(e) = std::fs::remove_file(path) {
				trace!(?path, "ignore remove error: {e}");
				0
			} else {
				trace!(?path, "removed");
				len
			}
		},
		| Err(_) => 0,
	}
}
