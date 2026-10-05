//! Replaces accepted timeline events with their room-version redacted form.
//!
//! Redaction can retain the original JSON for operators and removes searchable
//! or relational content before overwriting the accepted row. These side
//! effects are coordinated under the caller's room timeline guard.

use std::mem::take;

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, RoomId,
	canonical_json::{RedactedBecause, redact_in_place},
	room_version_rules::RedactionRules,
};
use tuwunel_core::{
	Result, err, implement,
	matrix::event::Event,
	utils::{BoolExt, OptionExt},
};

use crate::rooms::{
	short::ShortRoomId,
	threads::{thread_bundle_mut, thread_root},
	timeline::RoomMutexGuard,
};

/// Replaces an accepted PDU with its room-version redacted form.
///
/// Failure to resolve the event's accepted PDU ID is treated as a successful
/// no-op. Original retention, search removal, and relation deletion occur
/// before the accepted row is replaced, so the operation is not atomic if a
/// later step fails. A redacted thread reply is removed from its root's
/// `m.thread` summary (the count, and the latest event when it was the latest)
/// in the same write as the accepted row; a redacted thread root keeps its summary.
#[implement(super::Service)]
#[tracing::instrument(name = "redact", level = "debug", skip(self))]
pub async fn redact_pdu<Pdu: Event + Send + Sync>(
	&self,
	event_id: &EventId,
	reason: &Pdu,
	shortroomid: ShortRoomId,
	state_lock: &RoomMutexGuard,
) -> Result {
	let Ok(pdu_id) = self.get_pdu_id(event_id).await else {
		// If event does not exist, just noop
		// TODO this is actually wrong!
		return Ok(());
	};

	let pdu = self
		.get_pdu_json_from_id(&pdu_id)
		.await
		.map_err(|e| {
			err!(Database(error!(?pdu_id, ?event_id, ?e, "PDU ID points to invalid PDU.")))
		})?;

	self.services
		.retention
		.save_original_pdu(event_id, &pdu, state_lock)
		.await;

	let body = pdu["content"]
		.as_object()
		.and_then(|obj| obj.get("body"))
		.and_then(|body| body.as_str());

	if let Some(body) = body {
		self.services
			.search
			.deindex_pdu(shortroomid, &pdu_id, body);
	}

	let room_id: &RoomId = pdu.get("room_id").try_into()?;

	let room_version_id = self
		.services
		.state
		.get_room_version(room_id)
		.await?;

	let room_version_rules = room_version_id.rules().ok_or_else(|| {
		err!(Request(UnsupportedRoomVersion(
			"Cannot redact event for unknown room version {room_version_id:?}."
		)))
	})?;

	self.services
		.pdu_metadata
		.delete_typed_relation(&pdu_id, &pdu)
		.await;

	// Read before redaction strips `m.relates_to`.
	let root_event_id = pdu.get("content").and_then(thread_root);
	let keep_thread = root_event_id.as_deref() != Some(event_id);
	let pdu = redact_keeping_thread(
		pdu,
		keep_thread,
		&room_version_rules.redaction,
		RedactedBecause::from_json(reason.to_canonical_object()),
	)?;

	let root = root_event_id
		.as_deref()
		.map_async(|root| {
			self.services
				.threads
				.redacted_reply_root(root, &pdu_id, event_id)
		})
		.await
		.flatten();

	self.replace_redacted_pdu(&pdu_id, &pdu, root)
		.await
}

fn redact_keeping_thread(
	mut pdu: CanonicalJsonObject,
	keep_thread: bool,
	rules: &RedactionRules,
	because: RedactedBecause,
) -> Result<CanonicalJsonObject> {
	let thread = keep_thread.and_then(|| thread_bundle_mut(&mut pdu).map(take));

	redact_in_place(&mut pdu, rules, Some(because))
		.map_err(|err| err!("invalid event: {err}"))?;

	if let Some((thread, unsigned)) = thread.zip(
		pdu.get_mut("unsigned")
			.and_then(CanonicalJsonValue::as_object_mut),
	) {
		let relations = [("m.thread".into(), CanonicalJsonValue::Object(thread))].into();

		unsigned.insert("m.relations".into(), CanonicalJsonValue::Object(relations));
	}

	Ok(pdu)
}
