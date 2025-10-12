use std::{
	path::PathBuf,
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::StreamExt;
use ruma::UserId;
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, err, trace, warn};
use tuwunel_database::{Cbor, Deserialized, Map, keyval::serialize_val};

use super::Service;

/// keyspace prefixes inside the `media_retention` CF
const K_MREF: &str = "mref:"; // mref:<mxc>
const K_MER: &str = "mer:"; // mer:<event_id>:<kind>
const K_QUEUE: &str = "qdel:"; // qdel:<mxc> => DeletionCandidate

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetentionPolicy {
	Keep,
	DeleteIfUnreferenced,
	ForceDeleteLocal,
}

impl RetentionPolicy {
	pub(super) fn from_str(s: &str) -> Self {
		match s {
			| "delete_if_unreferenced" => Self::DeleteIfUnreferenced,
			| "force_delete_local" => Self::ForceDeleteLocal,
			| _ => Self::Keep,
		}
	}
}

#[derive(Clone)]
pub(super) struct Retention {
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

		let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(mxcs.len() * 2);
		for (mxc, local, kind) in mxcs.iter() {
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
		let mut processed = 0usize;

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
					| RetentionPolicy::DeleteIfUnreferenced => mr.refcount == 0,
					| RetentionPolicy::ForceDeleteLocal => mr.local,
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

	/// qeue a media item for deletion (idempotent best-effort).
	pub(super) fn queue_media_for_deletion(
		&self,
		mxc: &str,
		owner: Option<&UserId>,
		awaiting_confirmation: bool,
	) {
		let key = Self::key_queue(mxc);
		// overwrite / insert candidate with fresh timestamp
		let cand = DeletionCandidate {
			mxc: mxc.to_owned(),
			enqueued_ts: now_secs(),
			user_id: owner.map(|u| u.to_string()),
			awaiting_confirmation,
		};
		warn!(
			mxc,
			awaiting_confirmation,
			owner = owner.map(UserId::as_str),
			"retention: queue media for deletion"
		);
		self.cf.raw_put(key, Cbor(&cand));
	}

	pub(super) async fn confirm_candidate(
		&self,
		service: &Service,
		mxc: &str,
		requester: &UserId,
	) -> Result<u64> {
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
			return Err(err!(Request(InvalidParam(
				"media deletion already processed",
			))));
		}

		candidate.awaiting_confirmation = false;
		candidate.enqueued_ts = now_secs();

		let deleted_bytes = self.delete_local_media(service, mxc).await?;
		let mut dels = Vec::with_capacity(2);
		dels.push(key.into_bytes());
		dels.push(Self::key_mref(mxc).into_bytes());
		self.cf.write_batch_raw(std::iter::empty(), dels);
		warn!(mxc, bytes = deleted_bytes, user = requester.as_str(), "retention: media deletion confirmed by user");
		Ok(deleted_bytes)
	}

	/// worker: processes queued deletion candidates after grace period.
	pub(super) async fn worker_process_queue(
		&self,
		service: &Service,
		grace: Duration,
	) -> Result<()> {
		let prefix = K_QUEUE.as_bytes();
		warn!(?grace, "retention: worker iteration start");
		let mut stream = self
			.cf
			.stream_raw_prefix::<&str, Cbor<DeletionCandidate>, _>(&prefix);
		let mut processed = 0usize;
		let mut deleted = 0usize;
		while let Some(item) = stream.next().await.transpose()? {
			let (key, Cbor(cand)) = item;
			let now = now_secs();
			if cand.awaiting_confirmation {
				warn!(mxc = %cand.mxc, "retention: awaiting user confirmation, skipping candidate");
				continue;
			}

			if now < cand.enqueued_ts.saturating_add(grace.as_secs()) {
				warn!(mxc = %cand.mxc, wait = cand.enqueued_ts + grace.as_secs() - now, "retention: grace period not met yet");
				continue;
			}

			// attempt deletion of local media files
			let deleted_bytes = self
				.delete_local_media(service, &cand.mxc)
				.await
				.unwrap_or(0);
			if deleted_bytes > 0 {
				warn!(mxc = %cand.mxc, bytes = deleted_bytes, "retention: media deleted");
			} else {
				warn!(mxc = %cand.mxc, "retention: queued media had no bytes deleted (already gone?)");
			}

			// remove metadata entries (best-effort)
			let dels = vec![key.as_bytes().to_vec(), Self::key_mref(&cand.mxc).into_bytes()];
			self.cf.write_batch_raw(std::iter::empty(), dels);
			processed = processed.saturating_add(1);
			deleted = deleted.saturating_add(1);
		}
		if processed == 0 {
			warn!("retention: worker iteration found no deletion candidates");
		} else {
			warn!(processed, deleted, "retention: worker iteration complete");
		}

		Ok(())
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
		let mut total = 0u64;
		for key in keys {
			let path = service.get_media_file(&key);
			total = total.saturating_add(remove_file_tolerant(path));
			let legacy = service.get_media_file_b64(&key);
			total = total.saturating_add(remove_file_tolerant(legacy));
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

fn remove_file_tolerant(path: PathBuf) -> u64 {
	match std::fs::metadata(&path) {
		| Ok(meta) => {
			let len = meta.len();
			if let Err(e) = std::fs::remove_file(&path) {
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
