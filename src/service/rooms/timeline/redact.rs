//! Replaces accepted timeline events with their room-version redacted form.
//!
//! Redaction can retain the original JSON for operators and removes searchable
//! or relational content before overwriting the accepted row. These side
//! effects are coordinated under the caller's room timeline guard.

use ruma::{
	CanonicalJsonValue, EventId, RoomId,
	canonical_json::{RedactedBecause, redact_in_place},
};
use tuwunel_core::{Err, Result, err, implement, matrix::event::Event, utils::result::NotFound};

use crate::rooms::{
	short::ShortRoomId,
	threads::{thread_bundle, thread_root},
	timeline::RoomMutexGuard,
};

/// Replaces an accepted PDU with its room-version redacted form.
///
/// Failure to resolve the event's accepted PDU ID is treated as a successful
/// no-op. Original retention, search removal, and relation deletion occur
/// before the accepted row is replaced, so the operation is not atomic if a
/// later step fails. A thread reply's root loses the reply from its bundled
/// `m.thread.count` in the same write as the accepted row, and a redacted
/// thread root keeps that `m.thread` summary.
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

	let mut pdu = self
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
	let content = pdu.get("content").cloned();
	let root_event_id = content.and_then(|content| thread_root(content.into()));

	// Redaction replaces `unsigned`; a thread root keeps its thread summary,
	// unless it names itself as root and its summary quotes its own content.
	let thread = (root_event_id.as_deref() != Some(event_id))
		.then(|| thread_bundle(&mut pdu).map(|thread| thread.clone()))
		.flatten();

	redact_in_place(
		&mut pdu,
		&room_version_rules.redaction,
		Some(RedactedBecause::from_json(reason.to_canonical_object())),
	)
	.map_err(|err| err!("invalid event: {err}"))?;

	if let (Some(thread), Some(CanonicalJsonValue::Object(unsigned))) =
		(thread, pdu.get_mut("unsigned"))
	{
		let relations = [("m.thread".into(), CanonicalJsonValue::Object(thread))].into();

		unsigned.insert("m.relations".into(), CanonicalJsonValue::Object(relations));
	}

	// `replace_pdu`'s check; the reply and its root's count land together.
	let (pduid_pdu, mut txn) = (&self.db.pduid_pdu, self.db.db.txn());

	if pduid_pdu.get(&pdu_id).await.is_not_found() {
		return Err!(Request(NotFound("PDU does not exist.")));
	}

	if let Some(root_event_id) = root_event_id {
		self.services
			.threads
			.stage_redacted_reply(&mut txn, &root_event_id, &pdu_id)
			.await;
	}

	// Staged last, so the redacted form wins if the reply names itself as root.
	self.stage_replace_pdu(&mut txn, &pdu_id, &pdu);
	txn.execute();

	Ok(())
}
