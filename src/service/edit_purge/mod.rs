use std::{
	collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
	sync::Arc,
	time::Duration,
};

use async_trait::async_trait;
use futures::StreamExt;
use ruma::{
	Mxc, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, RoomId, ServerName, UserId,
};
use serde_json::Value as JsonValue;
use tokio::{
	sync::{Mutex, Notify},
	time::{MissedTickBehavior, interval},
};
use tuwunel_core::{
	Result, Server, debug, info,
	matrix::{
		event::{Event, ExtractRelatesToInfo},
		pdu::{PduEvent, RawPduId},
	},
	utils, warn,
};
use tuwunel_database::{Database, Map, serialize_to_vec};

use crate::rooms::timeline::bias_count;

pub struct Service {
	interval: Duration,
	interrupt: Notify,
	pduid_pdu: Arc<Map>,
	eventid_pduid: Arc<Map>,
	eventid_shorteventid: Arc<Map>,
	shorteventid_eventid: Arc<Map>,
	roomid_tscount_pducount: Arc<Map>,
	/// Last-scanned pdu_id key for incremental scanning.
	last_scan_key: Mutex<Option<Vec<u8>>>,
	/// Latest replacement seen so far for each (target, sender) during the
	/// current full-table scan pass. This is persisted across purge cycles so
	/// replacements split across scan windows are still compared.
	latest_replace_by_target_sender:
		Mutex<HashMap<(OwnedEventId, OwnedUserId), ReplaceCandidate>>,
	/// Superseded candidates discovered in previous cycles but not yet deleted
	/// because `batch_size` was reached.
	pending_superseded_candidates: Mutex<VecDeque<(OwnedEventId, ReplaceCandidate)>>,
	services: Services,
}

struct Services {
	server: Arc<Server>,
	db: Arc<Database>,
	media: SidecarMedia,
}

enum SidecarMedia {
	Runtime(Arc<crate::services::OnceServices>),
	#[cfg(test)]
	Test(Arc<dyn TestSidecarMedia>),
}

impl SidecarMedia {
	async fn mxc_is_owned_by_user(&self, mxc: &Mxc<'_>, user: &UserId) -> bool {
		match self {
			| Self::Runtime(services) =>
				services
					.media
					.mxc_is_owned_by_user(mxc, user)
					.await,
			#[cfg(test)]
			| Self::Test(media) => media.mxc_is_owned_by_user(mxc, user).await,
		}
	}

	async fn delete_owned_by(&self, mxc: &Mxc<'_>, user: &UserId) -> Result<bool> {
		match self {
			| Self::Runtime(services) => services.media.delete_owned_by(mxc, user).await,
			#[cfg(test)]
			| Self::Test(media) => media.delete_owned_by(mxc, user).await,
		}
	}
}

#[cfg(test)]
#[async_trait]
trait TestSidecarMedia: Send + Sync {
	async fn mxc_is_owned_by_user(&self, mxc: &Mxc<'_>, user: &UserId) -> bool;

	async fn delete_owned_by(&self, mxc: &Mxc<'_>, user: &UserId) -> Result<bool>;
}

/// A candidate replacement event with its metadata.
#[derive(Clone)]
struct ReplaceCandidate {
	/// event_id for deterministic tie-breaks.
	event_id: OwnedEventId,
	/// Sender of the replacement event, used to verify media ownership.
	sender: OwnedUserId,
	/// The raw PduId bytes for deletion from pduid_pdu.
	pdu_id_bytes: Vec<u8>,
	/// Room and timestamp for removal from roomid_tscount_pducount.
	room_id: OwnedRoomId,
	origin_server_ts_ms: u64,
	/// MindRoom long-text sidecar media referenced by this replacement event.
	sidecar_mxcs: Vec<OwnedMxcUri>,
}

fn compare_replace_candidates(a: &ReplaceCandidate, b: &ReplaceCandidate) -> std::cmp::Ordering {
	a.pdu_id_bytes
		.cmp(&b.pdu_id_bytes)
		.then_with(|| a.event_id.cmp(&b.event_id))
}

fn next_scan_start_key(key: &[u8]) -> Vec<u8> {
	let mut next = Vec::with_capacity(key.len().saturating_add(1));
	next.extend_from_slice(key);
	next.push(0);
	next
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let db = &args.db;
		Ok(Arc::new(Self {
			interval: Duration::from_secs(
				args.server
					.config
					.mindroom_edit_purge_interval_secs,
			),
			interrupt: Notify::new(),
			pduid_pdu: db["pduid_pdu"].clone(),
			eventid_pduid: db["eventid_pduid"].clone(),
			eventid_shorteventid: db["eventid_shorteventid"].clone(),
			shorteventid_eventid: db["shorteventid_eventid"].clone(),
			roomid_tscount_pducount: db["roomid_tscount_pducount"].clone(),
			last_scan_key: Mutex::new(None),
			latest_replace_by_target_sender: Mutex::new(HashMap::new()),
			pending_superseded_candidates: Mutex::new(VecDeque::new()),
			services: Services {
				server: args.server.clone(),
				db: args.db.clone(),
				media: SidecarMedia::Runtime(args.services.clone()),
			},
		}))
	}

	#[tracing::instrument(skip_all, name = "edit_purge", level = "debug")]
	async fn worker(self: Arc<Self>) -> Result {
		if !self
			.services
			.server
			.config
			.mindroom_edit_purge_enabled
		{
			debug!("MindRoom edit purge disabled");
			return Ok(());
		}

		if self.services.db.is_read_only() {
			warn!("MindRoom edit purge enabled but database is read-only; skipping purge worker");
			return Ok(());
		}

		info!(
			"MindRoom edit purge worker started (interval={}s, min_age={}s, batch={})",
			self.services
				.server
				.config
				.mindroom_edit_purge_interval_secs,
			self.services
				.server
				.config
				.mindroom_edit_purge_min_age_secs,
			self.services
				.server
				.config
				.mindroom_edit_purge_batch_size,
		);

		let mut i = interval(self.interval);
		i.set_missed_tick_behavior(MissedTickBehavior::Delay);
		i.reset_after(self.interval);
		let shutdown = self.services.server.until_shutdown();
		tokio::pin!(shutdown);
		loop {
			tokio::select! {
				() = self.interrupt.notified() => break,
				() = &mut shutdown => break,
				_ = i.tick() => (),
			}

			if let Err(e) = self.purge_cycle().await {
				warn!(%e, "MindRoom edit purge cycle failed");
			}
		}

		Ok(())
	}

	async fn interrupt(&self) { self.interrupt.notify_waiters(); }

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Run a single purge cycle: incrementally scan PDUs, find superseded
	/// m.replace events, and delete them with corked writes (batched flush),
	/// not as a single transactional unit.
	///
	/// Public so the maintenance worker, an operator, and integration tests can
	/// trigger a cycle on demand; it does not consult
	/// `mindroom_edit_purge_enabled` (the worker loop gates on that), only
	/// `mindroom_edit_purge_dry_run`.
	#[allow(clippy::too_many_lines)]
	#[tracing::instrument(skip_all, level = "debug")]
	pub async fn purge_cycle(&self) -> Result {
		let config = &self.services.server.config;
		let min_age_ms = config
			.mindroom_edit_purge_min_age_secs
			.saturating_mul(1000);
		let batch_size = config.mindroom_edit_purge_batch_size;
		let scan_limit = config
			.mindroom_edit_purge_batch_size
			.saturating_mul(10)
			.max(config.mindroom_edit_purge_scan_limit);
		let backlog_cap = scan_limit
			.max(batch_size.saturating_mul(32))
			.max(1_000);
		let dry_run = config.mindroom_edit_purge_dry_run;
		let now_ms = utils::millis_since_unix_epoch();
		let cutoff_ms = now_ms.saturating_sub(min_age_ms);

		// Phase 1: Incrementally scan PDUs from the last cursor position and
		// compare m.replace events against per-(target, sender) latest state that
		// persists across cycles during a full-table scan pass.
		let mut superseded_candidates: Vec<(OwnedEventId, ReplaceCandidate)> = Vec::new();
		let pending_len = self
			.pending_superseded_candidates
			.lock()
			.await
			.len();
		let should_scan = pending_len < backlog_cap;
		let mut latest_replace_by_target_sender =
			self.latest_replace_by_target_sender.lock().await;

		let mut last_key: Option<Vec<u8>> = None;
		let mut scanned: usize = 0;
		let mut reached_end = true;
		let mut reset_scan_state = false;

		if should_scan {
			let resume_key = self.last_scan_key.lock().await.clone();
			// raw_stream_from is inclusive, so move to the next lexicographic key
			// to avoid reprocessing the last key from the previous cycle.
			let resume_from_key = resume_key.as_deref().map(next_scan_start_key);

			let stream = resume_from_key
				.as_ref()
				.map_or_else(
					|| self.pduid_pdu.raw_stream().boxed(),
					|key| self.pduid_pdu.raw_stream_from(key).boxed(),
				)
				.peekable();
			tokio::pin!(stream);

			while let Some(kv) = stream.next().await {
				let Ok((key, value)) = kv else {
					continue;
				};

				last_key = Some(key.to_vec());
				scanned = scanned.saturating_add(1);

				if let Ok(pdu) = serde_json::from_slice::<PduEvent>(value)
					&& let Ok(content) = pdu.get_content::<ExtractRelatesToInfo>()
					&& content.relates_to.rel_type == "m.replace"
				{
					let ts: u64 = pdu.origin_server_ts.into();
					if ts <= cutoff_ms {
						let target_event_id = content.relates_to.event_id;
						let group_key = (target_event_id.clone(), pdu.sender.clone());
						let candidate = ReplaceCandidate {
							event_id: pdu.event_id.clone(),
							sender: pdu.sender.clone(),
							pdu_id_bytes: key.to_vec(),
							room_id: pdu.room_id.clone(),
							origin_server_ts_ms: ts,
							sidecar_mxcs: extract_mindroom_long_text_sidecar_mxcs(&pdu),
						};

						match latest_replace_by_target_sender.entry(group_key) {
							| Entry::Vacant(entry) => {
								entry.insert(candidate);
							},
							| Entry::Occupied(mut entry) => {
								if compare_replace_candidates(&candidate, entry.get()).is_gt() {
									let superseded =
										std::mem::replace(entry.get_mut(), candidate);
									superseded_candidates.push((target_event_id, superseded));
								} else {
									superseded_candidates.push((target_event_id, candidate));
								}
							},
						}
					}
				}

				// Backpressure: stop scanning when the pending queue is near capacity.
				if pending_len.saturating_add(superseded_candidates.len()) >= backlog_cap {
					reached_end = false;
					break;
				}

				// Limit scan size per cycle to avoid blocking too long;
				// we'll resume from this position next cycle.
				if scanned >= scan_limit {
					reached_end = stream.as_mut().peek().await.is_none();
					break;
				}
			}
		} else {
			debug!(
				backlog = pending_len,
				cap = backlog_cap,
				"MindRoom edit purge backlog is full; skipping scan this cycle"
			);
		}

		// Under sustained high write load this pass may never reach the end; cap
		// retained latest-state entries to avoid unbounded growth.
		let latest_state_cap = scan_limit.saturating_mul(4).max(10_000);
		if latest_replace_by_target_sender.len() > latest_state_cap {
			warn!(
				groups = latest_replace_by_target_sender.len(),
				cap = latest_state_cap,
				"MindRoom edit purge latest-state cache exceeded cap; resetting scan pass"
			);
			latest_replace_by_target_sender.clear();
			reset_scan_state = true;
		}

		// Update the cursor: if we reached the end, reset to None (start
		// over next cycle). Otherwise, save the last key for resuming.
		{
			let mut cursor = self.last_scan_key.lock().await;
			if !should_scan {
				// Preserve existing cursor when skipping scans due to backlog
				// pressure.
			} else if reached_end || reset_scan_state {
				*cursor = None;
				latest_replace_by_target_sender.clear();
			} else {
				*cursor = last_key;
			}
		}
		drop(latest_replace_by_target_sender);

		// Phase 2: Purge superseded events discovered during this and prior scan
		// windows.
		let mut purge_count: usize = 0;
		let mut target_ids: HashSet<OwnedEventId> = HashSet::new();
		let mut pending_superseded = self.pending_superseded_candidates.lock().await;
		pending_superseded.extend(superseded_candidates);
		let purge_budget = pending_superseded.len().min(batch_size);
		let purge_batch: Vec<_> = pending_superseded.drain(..purge_budget).collect();
		drop(pending_superseded);

		let cork = if !dry_run {
			Some(self.services.db.cork_and_flush())
		} else {
			None
		};

		let mut sidecar_cleanup_candidates: Vec<(OwnedEventId, ReplaceCandidate)> = Vec::new();
		for (target_event_id, candidate) in purge_batch {
			if dry_run {
				info!(
					event_id = %candidate.event_id,
					target = %target_event_id,
					"[dry-run] Would purge superseded edit"
				);
				purge_count = purge_count.saturating_add(1);
				target_ids.insert(target_event_id.clone());
				sidecar_cleanup_candidates.push((target_event_id, candidate));
				continue;
			}

			if self.delete_event(&candidate) {
				debug!(
					event_id = %candidate.event_id,
					target = %target_event_id,
					"Purged superseded edit event"
				);
				purge_count = purge_count.saturating_add(1);
				target_ids.insert(target_event_id.clone());
				sidecar_cleanup_candidates.push((target_event_id, candidate));
			} else {
				let mut pending = self.pending_superseded_candidates.lock().await;
				pending.push_back((target_event_id, candidate));
			}
		}
		drop(cork);

		let protected_mxcs = self
			.retained_referenced_mxcs(&sidecar_cleanup_candidates)
			.await;
		for (target_event_id, candidate) in sidecar_cleanup_candidates {
			self.process_sidecar_media(&target_event_id, &candidate, dry_run, &protected_mxcs)
				.await;
		}

		let target_count = target_ids.len();
		let remaining_backlog = self
			.pending_superseded_candidates
			.lock()
			.await
			.len();

		if purge_count > 0 || target_count > 0 || remaining_backlog > 0 {
			info!(
				"MindRoom edit purge: {}purged {purge_count} superseded edits for \
				 {target_count} target events (scanned {scanned} PDUs, backlog \
				 {remaining_backlog})",
				if dry_run { "[dry-run] would have " } else { "" },
			);
		} else {
			debug!("MindRoom edit purge: no superseded edits found (scanned {scanned} PDUs)");
		}

		Ok(())
	}

	/// Delete a superseded edit event from the database.
	///
	/// Returns true only when all targeted row/index removals succeeded.
	/// Returns false when purge should retry this candidate in a later cycle.
	fn delete_event(&self, candidate: &ReplaceCandidate) -> bool {
		// Remove from pduid_pdu (the main event storage)
		if let Err(e) = self
			.pduid_pdu
			.remove_fallible(&candidate.pdu_id_bytes)
		{
			warn!(
				%e,
				event_id = %candidate.event_id,
				"Failed to remove superseded edit from pduid_pdu; will retry candidate"
			);
			return false;
		}

		// Remove from eventid_pduid (event_id -> pdu_id index)
		if let Err(e) = self
			.eventid_pduid
			.remove_fallible(candidate.event_id.as_bytes())
		{
			warn!(
				%e,
				event_id = %candidate.event_id,
				"Failed to remove superseded edit from eventid_pduid; will retry candidate"
			);
			return false;
		}

		let pdu_count = RawPduId::from(candidate.pdu_id_bytes.as_slice()).count();
		let room_id: &RoomId = candidate.room_id.as_ref();
		let ts_index_key = match serialize_to_vec((
			room_id,
			candidate.origin_server_ts_ms,
			bias_count(pdu_count),
		)) {
			| Ok(key) => key,
			| Err(e) => {
				warn!(
					%e,
					event_id = %candidate.event_id,
					room_id = %candidate.room_id,
					ts = candidate.origin_server_ts_ms,
					"Failed to serialize room timestamp index key; will retry candidate"
				);
				return false;
			},
		};

		if self
			.roomid_tscount_pducount
			.get_blocking(&ts_index_key)
			.is_ok_and(|indexed_count| &*indexed_count == pdu_count.as_slice())
			&& let Err(e) = self
				.roomid_tscount_pducount
				.remove_fallible(&ts_index_key)
		{
			warn!(
				%e,
				event_id = %candidate.event_id,
				room_id = %candidate.room_id,
				ts = candidate.origin_server_ts_ms,
				"Failed to remove superseded edit from roomid_tscount_pducount; will retry candidate"
			);
			return false;
		}

		// Remove from eventid_shorteventid / shorteventid_eventid
		// (short numeric ID mappings that would otherwise be orphaned)
		if let Ok(short_bytes) = self
			.eventid_shorteventid
			.get_blocking(candidate.event_id.as_bytes())
			&& let Err(e) = self
				.shorteventid_eventid
				.remove_fallible(&*short_bytes)
		{
			warn!(
				%e,
				event_id = %candidate.event_id,
				"Failed to remove superseded edit from shorteventid_eventid; will retry candidate"
			);
			return false;
		}

		if let Err(e) = self
			.eventid_shorteventid
			.remove_fallible(candidate.event_id.as_bytes())
		{
			warn!(
				%e,
				event_id = %candidate.event_id,
				"Failed to remove superseded edit from eventid_shorteventid; will retry candidate"
			);
			return false;
		}

		// Note: we intentionally do NOT remove from tofrom_relation.
		// The relation entry is tiny (16 bytes) and removing it could
		// affect federation or relation queries. The PDU itself being
		// gone is sufficient — lookups via the relation will simply
		// fail to find the PDU and skip it.
		true
	}

	async fn process_sidecar_media(
		&self,
		target_event_id: &OwnedEventId,
		candidate: &ReplaceCandidate,
		dry_run: bool,
		protected_mxcs: &HashSet<OwnedMxcUri>,
	) {
		if candidate.sidecar_mxcs.is_empty() {
			return;
		}

		let local_server_name: &ServerName = self.services.server.name.as_ref();
		if candidate.sender.server_name() != local_server_name {
			warn!(
				event_id = %candidate.event_id,
				sender = %candidate.sender,
				target = %target_event_id,
				"Skipping MindRoom long-text sidecar cleanup for non-local edit sender"
			);
			return;
		}

		for mxc_uri in &candidate.sidecar_mxcs {
			if protected_mxcs.contains(mxc_uri) {
				debug!(
					event_id = %candidate.event_id,
					target = %target_event_id,
					mxc = %mxc_uri,
					"Skipping MindRoom long-text sidecar media deletion; MXC is still \
					 referenced by a retained event"
				);
				continue;
			}

			let Ok(mxc_server_name) = mxc_uri.server_name() else {
				warn!(
					event_id = %candidate.event_id,
					target = %target_event_id,
					mxc = %mxc_uri,
					"Skipping invalid MindRoom long-text sidecar MXC"
				);
				continue;
			};

			if mxc_server_name != local_server_name {
				debug!(
					event_id = %candidate.event_id,
					target = %target_event_id,
					mxc = %mxc_uri,
					"Skipping remote MindRoom long-text sidecar MXC"
				);
				continue;
			}

			let Ok(mxc) = Mxc::try_from(mxc_uri.as_str()) else {
				warn!(
					event_id = %candidate.event_id,
					target = %target_event_id,
					mxc = %mxc_uri,
					"Skipping unparsable MindRoom long-text sidecar MXC"
				);
				continue;
			};

			if dry_run {
				if self
					.services
					.media
					.mxc_is_owned_by_user(&mxc, &candidate.sender)
					.await
				{
					info!(
						event_id = %candidate.event_id,
						target = %target_event_id,
						mxc = %mxc_uri,
						"[dry-run] Would delete MindRoom long-text sidecar media"
					);
				} else {
					warn!(
						event_id = %candidate.event_id,
						target = %target_event_id,
						mxc = %mxc_uri,
						sender = %candidate.sender,
						"[dry-run] Would not delete MindRoom long-text sidecar media; \
						 ownership check failed"
					);
				}
				continue;
			}

			match self
				.services
				.media
				.delete_owned_by(&mxc, &candidate.sender)
				.await
			{
				| Ok(true) => {
					debug!(
						event_id = %candidate.event_id,
						target = %target_event_id,
						mxc = %mxc_uri,
						"Deleted MindRoom long-text sidecar media for purged edit"
					);
				},
				| Ok(false) => {
					warn!(
						event_id = %candidate.event_id,
						target = %target_event_id,
						mxc = %mxc_uri,
						sender = %candidate.sender,
						"Skipping MindRoom long-text sidecar media deletion; ownership check \
						 failed"
					);
				},
				| Err(e) => {
					warn!(
						%e,
						event_id = %candidate.event_id,
						target = %target_event_id,
						mxc = %mxc_uri,
						"Failed to delete MindRoom long-text sidecar media for purged edit"
					);
				},
			}
		}
	}

	async fn retained_referenced_mxcs(
		&self,
		cleanup_candidates: &[(OwnedEventId, ReplaceCandidate)],
	) -> HashSet<OwnedMxcUri> {
		let candidate_mxcs: HashSet<OwnedMxcUri> = cleanup_candidates
			.iter()
			.flat_map(|(_, candidate)| candidate.sidecar_mxcs.iter().cloned())
			.collect();

		if candidate_mxcs.is_empty() {
			return HashSet::new();
		}

		let ignored_event_ids: HashSet<OwnedEventId> = cleanup_candidates
			.iter()
			.map(|(_, candidate)| candidate.event_id.clone())
			.collect();
		let mut protected_mxcs = HashSet::new();
		let stream = self.pduid_pdu.raw_stream();
		tokio::pin!(stream);

		while let Some(kv) = stream.next().await {
			let Ok((_key, value)) = kv else {
				continue;
			};

			let Ok(pdu) = serde_json::from_slice::<PduEvent>(value) else {
				continue;
			};

			if ignored_event_ids.contains(&pdu.event_id) {
				continue;
			}

			let content = pdu.get_content_as_value();
			collect_referenced_candidate_mxcs(&content, &candidate_mxcs, &mut protected_mxcs);

			if protected_mxcs.len() == candidate_mxcs.len() {
				break;
			}
		}

		protected_mxcs
	}
}

fn extract_mindroom_long_text_sidecar_mxcs(pdu: &PduEvent) -> Vec<OwnedMxcUri> {
	let content = pdu.get_content_as_value();
	let mut mxcs = Vec::new();

	collect_mindroom_long_text_sidecar_mxcs(&content, &mut mxcs);
	if let Some(new_content) = content.get("m.new_content") {
		collect_mindroom_long_text_sidecar_mxcs(new_content, &mut mxcs);
	}

	mxcs
}

fn collect_mindroom_long_text_sidecar_mxcs(content: &JsonValue, mxcs: &mut Vec<OwnedMxcUri>) {
	if content.get("msgtype").and_then(JsonValue::as_str) != Some("m.file") {
		return;
	}

	let Some(long_text) = content.get("io.mindroom.long_text") else {
		return;
	};

	if long_text
		.get("version")
		.and_then(JsonValue::as_u64)
		!= Some(2)
		|| long_text
			.get("encoding")
			.and_then(JsonValue::as_str)
			!= Some("matrix_event_content_json")
	{
		return;
	}

	let url = content.get("url").and_then(JsonValue::as_str);
	let encrypted_url = content
		.get("file")
		.and_then(|file| file.get("url"))
		.and_then(JsonValue::as_str);

	for mxc in url.into_iter().chain(encrypted_url) {
		let mxc = OwnedMxcUri::from(mxc.to_owned());
		if mxc.is_valid() && !mxcs.iter().any(|existing| existing == &mxc) {
			mxcs.push(mxc);
		}
	}
}

fn collect_referenced_candidate_mxcs(
	value: &JsonValue,
	candidate_mxcs: &HashSet<OwnedMxcUri>,
	protected_mxcs: &mut HashSet<OwnedMxcUri>,
) {
	match value {
		| JsonValue::String(s) if s.starts_with("mxc://") => {
			let mxc = OwnedMxcUri::from(s.to_owned());
			if candidate_mxcs.contains(&mxc) {
				protected_mxcs.insert(mxc);
			}
		},
		| JsonValue::Array(items) =>
			for item in items {
				collect_referenced_candidate_mxcs(item, candidate_mxcs, protected_mxcs);
			},
		| JsonValue::Object(map) =>
			for item in map.values() {
				collect_referenced_candidate_mxcs(item, candidate_mxcs, protected_mxcs);
			},
		| _ => {},
	}
}

#[cfg(test)]
mod tests {
	use std::{
		collections::{HashMap, VecDeque},
		fs,
		path::{Path, PathBuf},
		sync::{
			Arc, Mutex as StdMutex, OnceLock,
			atomic::{AtomicU64, Ordering},
		},
		time::Duration,
	};

	use async_trait::async_trait;
	use ruma::{
		EventId, Mxc, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, RoomId, UInt, UserId,
	};
	use serde_json::value::RawValue;
	use tokio::{
		sync::{Mutex, MutexGuard, Notify},
		time::timeout,
	};
	use tracing::subscriber::NoSubscriber;
	use tuwunel_core::{
		Result, Server,
		config::Config,
		log::{Logging, capture::State as CaptureState},
		matrix::pdu::{EventHash, PduCount, PduEvent, PduId, RawPduId},
		metrics::Metrics,
		utils,
	};
	use tuwunel_database::{Database, serialize_to_vec};

	use super::{Service, Services, SidecarMedia, TestSidecarMedia, bias_count};

	static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);
	static TEST_DB_OPEN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

	#[derive(Clone, Copy)]
	struct HarnessConfig {
		min_age_secs: u64,
		batch_size: usize,
		scan_limit: usize,
		dry_run: bool,
		enabled: bool,
		read_only: bool,
	}

	impl Default for HarnessConfig {
		fn default() -> Self {
			Self {
				min_age_secs: 0,
				batch_size: 100,
				scan_limit: 100_000,
				dry_run: false,
				enabled: true,
				read_only: false,
			}
		}
	}

	struct TestHarness {
		service: Arc<Service>,
		media: Arc<TestMedia>,
		_temp_dir: PathBuf,
		// Keep this field last so database handles drop before the test lock is released.
		_db_guard: MutexGuard<'static, ()>,
	}

	#[derive(Default)]
	struct TestMedia {
		owners: StdMutex<HashMap<String, OwnedUserId>>,
	}

	#[async_trait]
	impl TestSidecarMedia for TestMedia {
		async fn mxc_is_owned_by_user(&self, mxc: &Mxc<'_>, user: &UserId) -> bool {
			self.owners
				.lock()
				.expect("test media lock")
				.get(&mxc.to_string())
				.is_some_and(|owner| owner == user)
		}

		async fn delete_owned_by(&self, mxc: &Mxc<'_>, user: &UserId) -> Result<bool> {
			let mxc = mxc.to_string();
			let mut owners = self.owners.lock().expect("test media lock");
			if owners.get(&mxc).is_none_or(|owner| owner != user) {
				return Ok(false);
			}

			owners.remove(&mxc);
			Ok(true)
		}
	}

	#[derive(Clone)]
	struct StoredEvent {
		event_id: OwnedEventId,
		room_id: OwnedRoomId,
		origin_server_ts_ms: u64,
		pdu_key: Vec<u8>,
		pdu_count: Vec<u8>,
		short_key: Vec<u8>,
	}

	async fn make_harness(cfg: HarnessConfig) -> TestHarness {
		let db_guard = TEST_DB_OPEN_LOCK
			.get_or_init(|| Mutex::new(()))
			.lock()
			.await;
		let temp_dir = unique_temp_dir();

		if cfg.read_only {
			let bootstrap_cfg = HarnessConfig { read_only: false, ..cfg };
			let (bootstrap_server, bootstrap_db) = open_server_db(&temp_dir, bootstrap_cfg).await;
			drop(bootstrap_db);
			drop(bootstrap_server);
		}

		let (server, db) = open_server_db(&temp_dir, cfg).await;
		let media = Arc::new(TestMedia::default());
		let service = Arc::new(Service {
			interval: Duration::from_secs(server.config.mindroom_edit_purge_interval_secs),
			interrupt: Notify::new(),
			pduid_pdu: db["pduid_pdu"].clone(),
			eventid_pduid: db["eventid_pduid"].clone(),
			eventid_shorteventid: db["eventid_shorteventid"].clone(),
			shorteventid_eventid: db["shorteventid_eventid"].clone(),
			roomid_tscount_pducount: db["roomid_tscount_pducount"].clone(),
			last_scan_key: Mutex::new(None),
			latest_replace_by_target_sender: Mutex::new(HashMap::new()),
			pending_superseded_candidates: Mutex::new(VecDeque::new()),
			services: Services {
				server,
				db,
				media: SidecarMedia::Test(media.clone()),
			},
		});

		TestHarness {
			service,
			media,
			_temp_dir: temp_dir,
			_db_guard: db_guard,
		}
	}

	async fn open_server_db(temp_dir: &Path, cfg: HarnessConfig) -> (Arc<Server>, Arc<Database>) {
		let db_path = temp_dir.join("db");
		let config_path = temp_dir.join(if cfg.read_only {
			"tuwunel-read-only.toml"
		} else {
			"tuwunel.toml"
		});

		fs::create_dir_all(temp_dir).expect("create test temp dir");
		let config_contents = format!(
			r#"[global]
server_name = "example.com"
database_path = "{}"
mindroom_edit_purge_enabled = {}
mindroom_edit_purge_min_age_secs = {}
mindroom_edit_purge_interval_secs = 60
mindroom_edit_purge_batch_size = {}
mindroom_edit_purge_scan_limit = {}
mindroom_edit_purge_dry_run = {}
rocksdb_read_only = {}
"#,
			db_path.display(),
			cfg.enabled,
			cfg.min_age_secs,
			cfg.batch_size,
			cfg.scan_limit,
			cfg.dry_run,
			cfg.read_only,
		);
		fs::write(&config_path, config_contents).expect("write test config");

		let figment = Config::load(std::iter::once(config_path.as_path())).expect("load config");
		let config = Config::new(&figment).expect("parse config");
		let log = Logging {
			reload: Default::default(),
			capture: Arc::new(CaptureState::new()),
			subscriber: Arc::new(NoSubscriber::new()),
		};
		let runtime = tokio::runtime::Handle::current();
		let metrics = Metrics::new(Some(&runtime));
		let server = Arc::new(Server::new(config, Some(&runtime), log, metrics));
		let db = Database::open(&server)
			.await
			.expect("open test database");

		(server, db)
	}

	fn unique_temp_dir() -> PathBuf {
		let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
		let pid = std::process::id();
		let path = std::env::temp_dir().join(format!("tuwunel-edit-purge-{pid}-{id}"));
		fs::create_dir_all(&path).expect("create unique test dir");
		path
	}

	fn pdu_key(index: u32) -> Vec<u8> {
		let raw = RawPduId::from(PduId {
			shortroomid: 1,
			count: PduCount::Normal(u64::from(index)),
		});
		raw.as_bytes().to_vec()
	}

	fn make_pdu(event_id: &str, sender: &str, ts: u64, replace_target: Option<&str>) -> PduEvent {
		let content = replace_target.map_or_else(
			|| r#"{"body":"original"}"#.to_owned(),
			|target| {
				format!(
					r#"{{"body":"edited","m.relates_to":{{"rel_type":"m.replace","event_id":"{target}"}}}}"#
				)
			},
		);

		make_pdu_with_content(event_id, sender, ts, content)
	}

	fn make_pdu_with_content(event_id: &str, sender: &str, ts: u64, content: String) -> PduEvent {
		PduEvent {
			kind: ruma::events::TimelineEventType::RoomMessage,
			content: RawValue::from_string(content)
				.expect("valid JSON content")
				.into(),
			event_id: EventId::parse(event_id).expect("valid event id"),
			room_id: OwnedRoomId::try_from("!room:example.com").expect("valid room id"),
			sender: OwnedUserId::try_from(sender).expect("valid sender"),
			state_key: None,
			redacts: None,
			prev_events: Default::default(),
			auth_events: Default::default(),
			origin_server_ts: UInt::try_from(ts).expect("valid timestamp"),
			depth: UInt::try_from(1_u64).expect("valid depth"),
			hashes: EventHash::default(),
			origin: None,
			unsigned: None,
		}
	}

	fn long_text_sidecar_content(target: &str, mxc: &str) -> String {
		format!(
			r#"{{
				"body":"message-content.json",
				"msgtype":"m.file",
				"url":"{mxc}",
				"io.mindroom.long_text":{{
					"version":2,
					"encoding":"matrix_event_content_json"
				}},
				"m.relates_to":{{
					"rel_type":"m.replace",
					"event_id":"{target}"
				}}
			}}"#
		)
	}

	fn encrypted_long_text_sidecar_content(target: &str, mxc: &str) -> String {
		format!(
			r#"{{
				"body":"message-content.json",
				"msgtype":"m.file",
				"file":{{"url":"{mxc}"}},
				"io.mindroom.long_text":{{
					"version":2,
					"encoding":"matrix_event_content_json"
				}},
				"m.relates_to":{{
					"rel_type":"m.replace",
					"event_id":"{target}"
				}}
			}}"#
		)
	}

	fn normal_file_content(target: &str, mxc: &str) -> String {
		format!(
			r#"{{
				"body":"ordinary-file.bin",
				"msgtype":"m.file",
				"url":"{mxc}",
				"m.relates_to":{{
					"rel_type":"m.replace",
					"event_id":"{target}"
				}}
			}}"#
		)
	}

	fn insert_event(
		service: &Service,
		key_index: u32,
		event_id: &str,
		sender: &str,
		ts: u64,
		replace_target: Option<&str>,
	) -> StoredEvent {
		let pdu = make_pdu(event_id, sender, ts, replace_target);
		let event_id = pdu.event_id.clone();
		let room_id = pdu.room_id.clone();
		let origin_server_ts_ms = u64::from(pdu.origin_server_ts);
		let key = pdu_key(key_index);
		let pdu_count = RawPduId::from(key.as_slice()).count();
		let short_key = key_index.to_be_bytes().to_vec();
		let value = serde_json::to_vec(&pdu).expect("serialize pdu");

		service.pduid_pdu.insert(&key, value);
		service
			.eventid_pduid
			.insert(event_id.as_bytes(), &key);
		service
			.eventid_shorteventid
			.insert(event_id.as_bytes(), &short_key);
		service
			.shorteventid_eventid
			.insert(&short_key, event_id.as_bytes());
		let room_id_ref: &RoomId = room_id.as_ref();
		service
			.roomid_tscount_pducount
			.put_raw((room_id_ref, origin_server_ts_ms, bias_count(pdu_count)), pdu_count);

		StoredEvent {
			event_id,
			room_id,
			origin_server_ts_ms,
			pdu_key: key,
			pdu_count: pdu_count.to_vec(),
			short_key,
		}
	}

	fn insert_event_with_content(
		service: &Service,
		key_index: u32,
		event_id: &str,
		sender: &str,
		ts: u64,
		content: String,
	) -> StoredEvent {
		let pdu = make_pdu_with_content(event_id, sender, ts, content);
		let event_id = pdu.event_id.clone();
		let room_id = pdu.room_id.clone();
		let origin_server_ts_ms = u64::from(pdu.origin_server_ts);
		let key = pdu_key(key_index);
		let pdu_count = RawPduId::from(key.as_slice()).count();
		let short_key = key_index.to_be_bytes().to_vec();
		let value = serde_json::to_vec(&pdu).expect("serialize pdu");

		service.pduid_pdu.insert(&key, value);
		service
			.eventid_pduid
			.insert(event_id.as_bytes(), &key);
		service
			.eventid_shorteventid
			.insert(event_id.as_bytes(), &short_key);
		service
			.shorteventid_eventid
			.insert(&short_key, event_id.as_bytes());
		let room_id_ref: &RoomId = room_id.as_ref();
		service
			.roomid_tscount_pducount
			.put_raw((room_id_ref, origin_server_ts_ms, bias_count(pdu_count)), pdu_count);

		StoredEvent {
			event_id,
			room_id,
			origin_server_ts_ms,
			pdu_key: key,
			pdu_count: pdu_count.to_vec(),
			short_key,
		}
	}

	fn assert_event_present(service: &Service, event: &StoredEvent) {
		assert!(
			service
				.pduid_pdu
				.get_blocking(&event.pdu_key)
				.is_ok(),
			"expected pduid_pdu entry for {}",
			event.event_id,
		);
		assert!(
			service
				.eventid_pduid
				.get_blocking(event.event_id.as_bytes())
				.is_ok(),
			"expected eventid_pduid entry for {}",
			event.event_id,
		);
		assert!(
			service
				.eventid_shorteventid
				.get_blocking(event.event_id.as_bytes())
				.is_ok(),
			"expected eventid_shorteventid entry for {}",
			event.event_id,
		);
		assert!(
			service
				.shorteventid_eventid
				.get_blocking(&event.short_key)
				.is_ok(),
			"expected shorteventid_eventid entry for {}",
			event.event_id,
		);
		let room_id: &RoomId = event.room_id.as_ref();
		let ts_index_key = serialize_to_vec((
			room_id,
			event.origin_server_ts_ms,
			bias_count(RawPduId::from(event.pdu_key.as_slice()).count()),
		))
		.expect("serialize timestamp index key");
		let indexed_count = service
			.roomid_tscount_pducount
			.get_blocking(&ts_index_key)
			.unwrap_or_else(|_| {
				panic!("expected roomid_tscount_pducount entry for {}", event.event_id)
			});
		assert_eq!(
			&*indexed_count,
			event.pdu_count.as_slice(),
			"expected timestamp index to point at {}",
			event.event_id,
		);
	}

	fn assert_event_absent(service: &Service, event: &StoredEvent) {
		assert!(
			service
				.pduid_pdu
				.get_blocking(&event.pdu_key)
				.is_err(),
			"expected no pduid_pdu entry for {}",
			event.event_id,
		);
		assert!(
			service
				.eventid_pduid
				.get_blocking(event.event_id.as_bytes())
				.is_err(),
			"expected no eventid_pduid entry for {}",
			event.event_id,
		);
		assert!(
			service
				.eventid_shorteventid
				.get_blocking(event.event_id.as_bytes())
				.is_err(),
			"expected no eventid_shorteventid entry for {}",
			event.event_id,
		);
		assert!(
			service
				.shorteventid_eventid
				.get_blocking(&event.short_key)
				.is_err(),
			"expected no shorteventid_eventid entry for {}",
			event.event_id,
		);
		let room_id: &RoomId = event.room_id.as_ref();
		let ts_index_key = serialize_to_vec((
			room_id,
			event.origin_server_ts_ms,
			bias_count(RawPduId::from(event.pdu_key.as_slice()).count()),
		))
		.expect("serialize timestamp index key");
		assert!(
			service
				.roomid_tscount_pducount
				.get_blocking(&ts_index_key)
				.is_err(),
			"expected no roomid_tscount_pducount entry for {}",
			event.event_id,
		);
	}

	fn assert_event_payload_indexes_absent(service: &Service, event: &StoredEvent) {
		assert!(
			service
				.pduid_pdu
				.get_blocking(&event.pdu_key)
				.is_err(),
			"expected no pduid_pdu entry for {}",
			event.event_id,
		);
		assert!(
			service
				.eventid_pduid
				.get_blocking(event.event_id.as_bytes())
				.is_err(),
			"expected no eventid_pduid entry for {}",
			event.event_id,
		);
		assert!(
			service
				.eventid_shorteventid
				.get_blocking(event.event_id.as_bytes())
				.is_err(),
			"expected no eventid_shorteventid entry for {}",
			event.event_id,
		);
		assert!(
			service
				.shorteventid_eventid
				.get_blocking(&event.short_key)
				.is_err(),
			"expected no shorteventid_eventid entry for {}",
			event.event_id,
		);
	}

	fn create_media_for_user(harness: &TestHarness, mxc: &str, user: &str) {
		Mxc::try_from(mxc).expect("valid MXC");
		let user = OwnedUserId::try_from(user).expect("valid user id");
		harness
			.media
			.owners
			.lock()
			.expect("test media lock")
			.insert(mxc.to_owned(), user);
	}

	fn assert_media_present(harness: &TestHarness, mxc: &str) {
		Mxc::try_from(mxc).expect("valid MXC");
		assert!(
			harness
				.media
				.owners
				.lock()
				.expect("test media lock")
				.contains_key(mxc),
			"expected media {mxc} to exist",
		);
	}

	fn assert_media_absent(harness: &TestHarness, mxc: &str) {
		Mxc::try_from(mxc).expect("valid MXC");
		assert!(
			!harness
				.media
				.owners
				.lock()
				.expect("test media lock")
				.contains_key(mxc),
			"expected media {mxc} to be absent",
		);
	}

	#[test]
	fn extract_long_text_sidecar_mxcs_deduplicates_plain_and_encrypted_urls() {
		let sidecar_mxc = "mxc://example.com/dedupSidecar";
		let pdu = make_pdu_with_content(
			"$edit_sidecar_extract:example.com",
			"@alice:example.com",
			1_000,
			format!(
				r#"{{
					"body":"message-content.json",
					"msgtype":"m.file",
					"url":"{sidecar_mxc}",
					"file":{{"url":"{sidecar_mxc}"}},
					"io.mindroom.long_text":{{
						"version":2,
						"encoding":"matrix_event_content_json"
					}},
					"m.relates_to":{{
						"rel_type":"m.replace",
						"event_id":"$target_sidecar_extract:example.com"
					}}
				}}"#
			),
		);

		let mxcs = super::extract_mindroom_long_text_sidecar_mxcs(&pdu);

		assert_eq!(mxcs, vec![OwnedMxcUri::from(sidecar_mxc.to_owned())]);
	}

	#[tokio::test]
	async fn purge_basic_keeps_only_latest_edit() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;

		let target = insert_event(
			service,
			0,
			"$target_basic:example.com",
			"@alice:example.com",
			100,
			None,
		);
		let edit1 = insert_event(
			service,
			1,
			"$edit_basic_1:example.com",
			"@alice:example.com",
			1_000,
			Some("$target_basic:example.com"),
		);
		let edit2 = insert_event(
			service,
			2,
			"$edit_basic_2:example.com",
			"@alice:example.com",
			2_000,
			Some("$target_basic:example.com"),
		);
		let edit3 = insert_event(
			service,
			3,
			"$edit_basic_3:example.com",
			"@alice:example.com",
			3_000,
			Some("$target_basic:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &edit1);
		assert_event_absent(service, &edit2);
		assert_event_present(service, &edit3);
	}

	#[tokio::test]
	async fn purge_prefers_pdu_order_over_timestamp() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;

		let target = insert_event(
			service,
			0,
			"$target_order:example.com",
			"@alice:example.com",
			100,
			None,
		);
		let newer_by_count_older_ts = insert_event(
			service,
			2,
			"$edit_order_keep:example.com",
			"@alice:example.com",
			1_000,
			Some("$target_order:example.com"),
		);
		let older_by_count_newer_ts = insert_event(
			service,
			1,
			"$edit_order_drop:example.com",
			"@alice:example.com",
			4_000,
			Some("$target_order:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_present(service, &newer_by_count_older_ts);
		assert_event_absent(service, &older_by_count_newer_ts);
	}

	#[tokio::test]
	async fn purge_preserves_timestamp_index_for_retained_edit_with_same_timestamp() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;
		let shared_edit_ts = 1_000;

		let target = insert_event(
			service,
			0,
			"$target_same_ts:example.com",
			"@alice:example.com",
			100,
			None,
		);
		let old_edit = insert_event(
			service,
			1,
			"$edit_same_ts_old:example.com",
			"@alice:example.com",
			shared_edit_ts,
			Some("$target_same_ts:example.com"),
		);
		let latest_edit = insert_event(
			service,
			2,
			"$edit_same_ts_new:example.com",
			"@alice:example.com",
			shared_edit_ts,
			Some("$target_same_ts:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_payload_indexes_absent(service, &old_edit);
		assert_event_present(service, &latest_edit);
	}

	#[tokio::test]
	async fn purge_respects_min_age_threshold() {
		let harness = make_harness(HarnessConfig {
			min_age_secs: 60,
			..HarnessConfig::default()
		})
		.await;
		let service = &harness.service;
		let now_ms = utils::millis_since_unix_epoch();

		let target = insert_event(
			service,
			0,
			"$target_min_age:example.com",
			"@alice:example.com",
			now_ms.saturating_sub(2_000),
			None,
		);
		let recent_edit1 = insert_event(
			service,
			1,
			"$edit_recent_1:example.com",
			"@alice:example.com",
			now_ms.saturating_sub(1_000),
			Some("$target_min_age:example.com"),
		);
		let recent_edit2 = insert_event(
			service,
			2,
			"$edit_recent_2:example.com",
			"@alice:example.com",
			now_ms.saturating_sub(500),
			Some("$target_min_age:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_present(service, &recent_edit1);
		assert_event_present(service, &recent_edit2);
	}

	#[tokio::test]
	async fn purge_groups_edits_by_sender() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;

		let target = insert_event(
			service,
			0,
			"$target_sender:example.com",
			"@alice:example.com",
			100,
			None,
		);
		let alice_edit1 = insert_event(
			service,
			1,
			"$edit_sender_alice_1:example.com",
			"@alice:example.com",
			1_000,
			Some("$target_sender:example.com"),
		);
		let bob_edit1 = insert_event(
			service,
			2,
			"$edit_sender_bob_1:example.com",
			"@bob:example.com",
			1_500,
			Some("$target_sender:example.com"),
		);
		let alice_edit2 = insert_event(
			service,
			3,
			"$edit_sender_alice_2:example.com",
			"@alice:example.com",
			2_000,
			Some("$target_sender:example.com"),
		);
		let bob_edit2 = insert_event(
			service,
			4,
			"$edit_sender_bob_2:example.com",
			"@bob:example.com",
			2_500,
			Some("$target_sender:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &alice_edit1);
		assert_event_present(service, &alice_edit2);
		assert_event_absent(service, &bob_edit1);
		assert_event_present(service, &bob_edit2);
	}

	#[tokio::test]
	async fn purge_groups_edits_by_target() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;

		let target1 = insert_event(
			service,
			0,
			"$target_multi_a:example.com",
			"@alice:example.com",
			100,
			None,
		);
		let target2 = insert_event(
			service,
			1,
			"$target_multi_b:example.com",
			"@alice:example.com",
			200,
			None,
		);

		let target1_edit1 = insert_event(
			service,
			2,
			"$edit_multi_a_1:example.com",
			"@alice:example.com",
			1_000,
			Some("$target_multi_a:example.com"),
		);
		let target1_edit2 = insert_event(
			service,
			3,
			"$edit_multi_a_2:example.com",
			"@alice:example.com",
			2_000,
			Some("$target_multi_a:example.com"),
		);
		let target2_edit1 = insert_event(
			service,
			4,
			"$edit_multi_b_1:example.com",
			"@alice:example.com",
			1_500,
			Some("$target_multi_b:example.com"),
		);
		let target2_edit2 = insert_event(
			service,
			5,
			"$edit_multi_b_2:example.com",
			"@alice:example.com",
			2_500,
			Some("$target_multi_b:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target1);
		assert_event_present(service, &target2);
		assert_event_absent(service, &target1_edit1);
		assert_event_present(service, &target1_edit2);
		assert_event_absent(service, &target2_edit1);
		assert_event_present(service, &target2_edit2);
	}

	#[tokio::test]
	async fn purge_single_edit_is_not_removed() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;

		let target = insert_event(
			service,
			0,
			"$target_single:example.com",
			"@alice:example.com",
			100,
			None,
		);
		let single_edit = insert_event(
			service,
			1,
			"$edit_single:example.com",
			"@alice:example.com",
			1_000,
			Some("$target_single:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_present(service, &single_edit);
	}

	#[tokio::test]
	async fn purge_dry_run_does_not_delete_events() {
		let harness = make_harness(HarnessConfig {
			dry_run: true,
			..HarnessConfig::default()
		})
		.await;
		let service = &harness.service;
		let sidecar_mxc = "mxc://example.com/dryRunSidecar";

		let target = insert_event(
			service,
			0,
			"$target_dry_run:example.com",
			"@alice:example.com",
			100,
			None,
		);
		create_media_for_user(&harness, sidecar_mxc, "@alice:example.com");
		let edit1 = insert_event_with_content(
			service,
			1,
			"$edit_dry_run_1:example.com",
			"@alice:example.com",
			1_000,
			long_text_sidecar_content("$target_dry_run:example.com", sidecar_mxc),
		);
		let edit2 = insert_event(
			service,
			2,
			"$edit_dry_run_2:example.com",
			"@alice:example.com",
			2_000,
			Some("$target_dry_run:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_present(service, &edit1);
		assert_event_present(service, &edit2);
		assert_media_present(&harness, sidecar_mxc);
	}

	#[tokio::test]
	async fn purge_deletes_superseded_mindroom_long_text_sidecar_media() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;
		let sidecar_mxc = "mxc://example.com/supersededSidecar";

		let target = insert_event(
			service,
			0,
			"$target_sidecar_delete:example.com",
			"@alice:example.com",
			100,
			None,
		);
		create_media_for_user(&harness, sidecar_mxc, "@alice:example.com");
		let old_edit = insert_event_with_content(
			service,
			1,
			"$edit_sidecar_delete_old:example.com",
			"@alice:example.com",
			1_000,
			long_text_sidecar_content("$target_sidecar_delete:example.com", sidecar_mxc),
		);
		let latest_edit = insert_event(
			service,
			2,
			"$edit_sidecar_delete_latest:example.com",
			"@alice:example.com",
			2_000,
			Some("$target_sidecar_delete:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &old_edit);
		assert_event_present(service, &latest_edit);
		assert_media_absent(&harness, sidecar_mxc);
	}

	#[tokio::test]
	async fn purge_preserves_latest_edit_sidecar_media() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;
		let old_mxc = "mxc://example.com/oldSidecar";
		let latest_mxc = "mxc://example.com/latestSidecar";

		let target = insert_event(
			service,
			0,
			"$target_sidecar_latest:example.com",
			"@alice:example.com",
			100,
			None,
		);
		create_media_for_user(&harness, old_mxc, "@alice:example.com");
		create_media_for_user(&harness, latest_mxc, "@alice:example.com");
		let old_edit = insert_event_with_content(
			service,
			1,
			"$edit_sidecar_latest_old:example.com",
			"@alice:example.com",
			1_000,
			long_text_sidecar_content("$target_sidecar_latest:example.com", old_mxc),
		);
		let latest_edit = insert_event_with_content(
			service,
			2,
			"$edit_sidecar_latest_new:example.com",
			"@alice:example.com",
			2_000,
			encrypted_long_text_sidecar_content("$target_sidecar_latest:example.com", latest_mxc),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &old_edit);
		assert_event_present(service, &latest_edit);
		assert_media_absent(&harness, old_mxc);
		assert_media_present(&harness, latest_mxc);
	}

	#[tokio::test]
	async fn purge_preserves_sidecar_reused_by_latest_edit() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;
		let shared_mxc = "mxc://example.com/reusedLatestSidecar";

		let target = insert_event(
			service,
			0,
			"$target_sidecar_reused:example.com",
			"@alice:example.com",
			100,
			None,
		);
		create_media_for_user(&harness, shared_mxc, "@alice:example.com");
		let old_edit = insert_event_with_content(
			service,
			1,
			"$edit_sidecar_reused_old:example.com",
			"@alice:example.com",
			1_000,
			long_text_sidecar_content("$target_sidecar_reused:example.com", shared_mxc),
		);
		let latest_edit = insert_event_with_content(
			service,
			2,
			"$edit_sidecar_reused_new:example.com",
			"@alice:example.com",
			2_000,
			long_text_sidecar_content("$target_sidecar_reused:example.com", shared_mxc),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &old_edit);
		assert_event_present(service, &latest_edit);
		assert_media_present(&harness, shared_mxc);
	}

	#[tokio::test]
	async fn purge_preserves_sidecar_reused_by_pending_edit() {
		let harness = make_harness(HarnessConfig {
			batch_size: 1,
			..HarnessConfig::default()
		})
		.await;
		let service = &harness.service;
		let shared_mxc = "mxc://example.com/reusedPendingSidecar";

		let target = insert_event(
			service,
			0,
			"$target_sidecar_pending:example.com",
			"@alice:example.com",
			100,
			None,
		);
		create_media_for_user(&harness, shared_mxc, "@alice:example.com");
		let old_edit = insert_event_with_content(
			service,
			1,
			"$edit_sidecar_pending_old:example.com",
			"@alice:example.com",
			1_000,
			long_text_sidecar_content("$target_sidecar_pending:example.com", shared_mxc),
		);
		let pending_edit = insert_event_with_content(
			service,
			2,
			"$edit_sidecar_pending_mid:example.com",
			"@alice:example.com",
			2_000,
			long_text_sidecar_content("$target_sidecar_pending:example.com", shared_mxc),
		);
		let latest_edit = insert_event(
			service,
			3,
			"$edit_sidecar_pending_latest:example.com",
			"@alice:example.com",
			3_000,
			Some("$target_sidecar_pending:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &old_edit);
		assert_event_present(service, &pending_edit);
		assert_event_present(service, &latest_edit);
		assert_media_present(&harness, shared_mxc);

		service
			.purge_cycle()
			.await
			.expect("second purge cycle succeeds");

		assert_event_absent(service, &pending_edit);
		assert_event_present(service, &latest_edit);
		assert_media_absent(&harness, shared_mxc);
	}

	#[tokio::test]
	async fn purge_preserves_sidecar_reused_by_recent_edit() {
		let harness = make_harness(HarnessConfig {
			min_age_secs: 60,
			..HarnessConfig::default()
		})
		.await;
		let service = &harness.service;
		let now_ms = utils::millis_since_unix_epoch();
		let shared_mxc = "mxc://example.com/reusedRecentSidecar";

		let target = insert_event(
			service,
			0,
			"$target_sidecar_recent:example.com",
			"@alice:example.com",
			now_ms.saturating_sub(130_000),
			None,
		);
		create_media_for_user(&harness, shared_mxc, "@alice:example.com");
		let old_edit = insert_event_with_content(
			service,
			1,
			"$edit_sidecar_recent_old:example.com",
			"@alice:example.com",
			now_ms.saturating_sub(120_000),
			long_text_sidecar_content("$target_sidecar_recent:example.com", shared_mxc),
		);
		let older_latest_edit = insert_event(
			service,
			2,
			"$edit_sidecar_recent_mid:example.com",
			"@alice:example.com",
			now_ms.saturating_sub(110_000),
			Some("$target_sidecar_recent:example.com"),
		);
		let recent_edit = insert_event_with_content(
			service,
			3,
			"$edit_sidecar_recent_new:example.com",
			"@alice:example.com",
			now_ms.saturating_sub(1_000),
			long_text_sidecar_content("$target_sidecar_recent:example.com", shared_mxc),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &old_edit);
		assert_event_present(service, &older_latest_edit);
		assert_event_present(service, &recent_edit);
		assert_media_present(&harness, shared_mxc);
	}

	#[tokio::test]
	async fn purge_does_not_delete_normal_file_attachment_media() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;
		let attachment_mxc = "mxc://example.com/ordinaryAttachment";

		let target = insert_event(
			service,
			0,
			"$target_normal_file:example.com",
			"@alice:example.com",
			100,
			None,
		);
		create_media_for_user(&harness, attachment_mxc, "@alice:example.com");
		let old_edit = insert_event_with_content(
			service,
			1,
			"$edit_normal_file_old:example.com",
			"@alice:example.com",
			1_000,
			normal_file_content("$target_normal_file:example.com", attachment_mxc),
		);
		let latest_edit = insert_event(
			service,
			2,
			"$edit_normal_file_latest:example.com",
			"@alice:example.com",
			2_000,
			Some("$target_normal_file:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &old_edit);
		assert_event_present(service, &latest_edit);
		assert_media_present(&harness, attachment_mxc);
	}

	#[tokio::test]
	async fn purge_does_not_delete_sidecar_owned_by_different_user() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;
		let sidecar_mxc = "mxc://example.com/notAliceSidecar";

		let target = insert_event(
			service,
			0,
			"$target_sidecar_owner:example.com",
			"@alice:example.com",
			100,
			None,
		);
		create_media_for_user(&harness, sidecar_mxc, "@bob:example.com");
		let old_edit = insert_event_with_content(
			service,
			1,
			"$edit_sidecar_owner_old:example.com",
			"@alice:example.com",
			1_000,
			long_text_sidecar_content("$target_sidecar_owner:example.com", sidecar_mxc),
		);
		let latest_edit = insert_event(
			service,
			2,
			"$edit_sidecar_owner_latest:example.com",
			"@alice:example.com",
			2_000,
			Some("$target_sidecar_owner:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &old_edit);
		assert_event_present(service, &latest_edit);
		assert_media_present(&harness, sidecar_mxc);
	}

	#[tokio::test]
	async fn purge_does_not_delete_remote_sidecar_media() {
		let harness = make_harness(HarnessConfig::default()).await;
		let service = &harness.service;
		let remote_mxc = "mxc://remote.example/remoteSidecar";

		let target = insert_event(
			service,
			0,
			"$target_sidecar_remote:example.com",
			"@alice:example.com",
			100,
			None,
		);
		create_media_for_user(&harness, remote_mxc, "@alice:example.com");
		let old_edit = insert_event_with_content(
			service,
			1,
			"$edit_sidecar_remote_old:example.com",
			"@alice:example.com",
			1_000,
			long_text_sidecar_content("$target_sidecar_remote:example.com", remote_mxc),
		);
		let latest_edit = insert_event(
			service,
			2,
			"$edit_sidecar_remote_latest:example.com",
			"@alice:example.com",
			2_000,
			Some("$target_sidecar_remote:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &old_edit);
		assert_event_present(service, &latest_edit);
		assert_media_present(&harness, remote_mxc);
	}

	#[tokio::test]
	async fn purge_respects_batch_size_limit() {
		let harness = make_harness(HarnessConfig {
			batch_size: 2,
			..HarnessConfig::default()
		})
		.await;
		let service = &harness.service;

		let target = insert_event(
			service,
			0,
			"$target_batch:example.com",
			"@alice:example.com",
			100,
			None,
		);
		let edit1 = insert_event(
			service,
			1,
			"$edit_batch_1:example.com",
			"@alice:example.com",
			1_000,
			Some("$target_batch:example.com"),
		);
		let edit2 = insert_event(
			service,
			2,
			"$edit_batch_2:example.com",
			"@alice:example.com",
			2_000,
			Some("$target_batch:example.com"),
		);
		let edit3 = insert_event(
			service,
			3,
			"$edit_batch_3:example.com",
			"@alice:example.com",
			3_000,
			Some("$target_batch:example.com"),
		);
		let edit4 = insert_event(
			service,
			4,
			"$edit_batch_4:example.com",
			"@alice:example.com",
			4_000,
			Some("$target_batch:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("purge cycle succeeds");

		assert_event_present(service, &target);
		assert_event_absent(service, &edit1);
		assert_event_absent(service, &edit2);
		assert_event_present(service, &edit3);
		assert_event_present(service, &edit4);

		// Remaining superseded candidates should stay in backlog and purge on
		// subsequent cycles.
		service
			.purge_cycle()
			.await
			.expect("second purge cycle succeeds");

		assert_event_absent(service, &edit3);
		assert_event_present(service, &edit4);
	}

	#[tokio::test]
	async fn purge_across_scan_windows_persists_latest_state() {
		let harness = make_harness(HarnessConfig {
			batch_size: 1,
			scan_limit: 10,
			..HarnessConfig::default()
		})
		.await;
		let service = &harness.service;
		assert_eq!(
			service
				.services
				.server
				.config
				.mindroom_edit_purge_batch_size,
			1
		);
		assert_eq!(
			service
				.services
				.server
				.config
				.mindroom_edit_purge_scan_limit,
			10
		);

		let target = insert_event(
			service,
			0,
			"$target_windows:example.com",
			"@alice:example.com",
			100,
			None,
		);
		let older_edit = insert_event(
			service,
			1,
			"$edit_windows_old:example.com",
			"@alice:example.com",
			1_000,
			Some("$target_windows:example.com"),
		);

		{
			let _cork = service.services.db.cork_and_flush();
			for i in 2..=18_u32 {
				service
					.pduid_pdu
					.insert(&pdu_key(i), br"not-json");
			}
		}

		let newer_edit = insert_event(
			service,
			19,
			"$edit_windows_new:example.com",
			"@alice:example.com",
			2_000,
			Some("$target_windows:example.com"),
		);

		service
			.purge_cycle()
			.await
			.expect("first purge cycle succeeds");
		assert_event_present(service, &target);
		assert_event_present(service, &older_edit);
		assert!(service.last_scan_key.lock().await.is_some());

		service
			.purge_cycle()
			.await
			.expect("second purge cycle succeeds");

		assert_event_absent(service, &older_edit);
		assert_event_present(service, &newer_edit);
		assert!(service.last_scan_key.lock().await.is_none());
	}

	#[tokio::test]
	async fn worker_exits_gracefully_when_database_is_read_only() {
		let harness = make_harness(HarnessConfig {
			read_only: true,
			..HarnessConfig::default()
		})
		.await;

		let result = timeout(
			Duration::from_secs(1),
			<Service as crate::Service>::worker(harness.service.clone()),
		)
		.await;

		assert!(result.is_ok(), "worker timed out on read-only database");
		assert!(
			result.expect("timeout already checked").is_ok(),
			"worker should return Ok on read-only database"
		);
	}
}
