//! Replaces accepted timeline events with their room-version redacted form.
//!
//! Redaction can retain the original JSON for operators and removes searchable
//! or relational content before overwriting the accepted row. These side
//! effects are coordinated under the caller's room timeline guard.

use std::mem::take;

use futures::future::join;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, RoomId,
	canonical_json::{RedactedBecause, redact_in_place},
	events::TimelineEventType,
	room_version_rules::RedactionRules,
};
use tuwunel_core::{
	Result, err, implement,
	matrix::event::Event,
	utils::{BoolExt, OptionExt, result::NotFound, to_canonical_object},
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
		redacted_because(reason.to_canonical_object()),
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

/// Records an applied redaction on an accepted PDU that does not carry it.
///
/// A database migrated from another homeserver implementation can hold an event
/// whose content a redaction already removed but which lacks `redacted_because`,
/// so it reads as unredacted. When the content already equals its redacted
/// form, the row is rewritten in the form `redact_pdu` leaves, skipping the
/// steps that need the original content. As with `redact_pdu`, the latest of
/// several redactions is the one recorded. Returns whether the row was
/// rewritten.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, reason, rules, _state_lock))]
pub async fn restore_redaction<Pdu: Event + Send + Sync>(
	&self,
	event_id: &EventId,
	reason: &Pdu,
	rules: &RedactionRules,
	_state_lock: &RoomMutexGuard,
) -> Result<bool> {
	let Some(pdu_id) = self.get_pdu_id(event_id).await.optional()? else {
		return Ok(false);
	};

	let pdu = self.get_pdu_json_from_id(&pdu_id).await?;

	if !restorable(&pdu, reason.room_id()) {
		return Ok(false);
	}

	if let Some(recorded) = recorded_redaction(&pdu)
		&& !self
			.redaction_precedes(recorded, reason.event_id())
			.await?
	{
		return Ok(false);
	}

	let because = to_canonical_object(reason.as_pdu()).map(redacted_because)?;
	let Some(pdu) = already_redacted(pdu, rules, because)? else {
		return Ok(false);
	};

	self.replace_redacted_pdu(&pdu_id, &pdu, None)
		.await
		.map(|()| true)
}

/// Whether the redaction a row records comes before `later` in the timeline.
///
/// A missing or malformed record, or a missing timeline position, is kept.
/// Other lookup failures are returned to the caller.
#[implement(super::Service)]
async fn redaction_precedes(
	&self,
	recorded: &CanonicalJsonValue,
	later: &EventId,
) -> Result<bool> {
	let Some(recorded) = recorded
		.as_object()
		.and_then(|recorded| recorded.get("event_id"))
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|id| <&EventId>::try_from(id).ok())
	else {
		return Ok(false);
	};

	let (recorded, later) = join(self.get_pdu_count(recorded), self.get_pdu_count(later)).await;
	let precedes = recorded
		.optional()?
		.zip(later.optional()?)
		.is_some_and(|(recorded, later)| recorded < later);

	Ok(precedes)
}

/// Whether a stored event in the room is of a type a redaction can apply to.
///
/// This server and the implementations it migrates from refuse to redact a
/// create or server ACL event whatever the sender's power, so no such row was
/// redacted.
fn restorable(pdu: &CanonicalJsonObject, room_id: &RoomId) -> bool {
	let in_room = pdu
		.get("room_id")
		.and_then(CanonicalJsonValue::as_str)
		.is_some_and(|id| id == room_id.as_str());

	let redactable = pdu
		.get("type")
		.and_then(CanonicalJsonValue::as_str)
		.map(TimelineEventType::from)
		.is_none_or(|kind| {
			!matches!(kind, TimelineEventType::RoomCreate | TimelineEventType::RoomServerAcl)
		});

	in_room && redactable
}

fn recorded_redaction(pdu: &CanonicalJsonObject) -> Option<&CanonicalJsonValue> {
	pdu.get("unsigned")
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|unsigned| unsigned.get("redacted_because"))
}

/// The redacted form of an event whose content the redaction already removed.
///
/// `None` when redacting would still remove content, so no redaction has been
/// applied to it.
fn already_redacted(
	pdu: CanonicalJsonObject,
	rules: &RedactionRules,
	because: RedactedBecause,
) -> Result<Option<CanonicalJsonObject>> {
	let content = pdu.get("content").cloned();
	let redacted = redact_keeping_thread(pdu, true, rules, because)?;
	let unchanged = redacted.get("content") == content.as_ref();

	Ok(unchanged.then_some(redacted))
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

/// The redaction as recorded on the event it redacts, without its own `unsigned`.
///
/// That field holds what the server knows about the redaction, such as the
/// sender's transaction id, which is not for the readers of the redacted event.
fn redacted_because(mut reason: CanonicalJsonObject) -> RedactedBecause {
	reason.remove("unsigned");
	RedactedBecause::from_json(reason)
}

#[cfg(test)]
mod tests {
	use ruma::{RoomVersionId, room_id};
	use serde_json::{from_value, json};

	use super::*;

	#[test]
	fn recorded_redaction_drops_its_unsigned() {
		let target = object(json!({
			"type": "m.room.message",
			"content": { "body": "hello", "msgtype": "m.text" },
			"unsigned": { "transaction_id": "target-txn" },
		}));

		let reason = object(json!({
			"type": "m.room.redaction",
			"content": { "redacts": "$target" },
			"unsigned": { "transaction_id": "redactor-txn" },
		}));

		let redacted = redact_keeping_thread(target, true, &rules(), redacted_because(reason))
			.expect("redactable event");

		let expected = object(json!({
			"redacted_because": {
				"type": "m.room.redaction",
				"content": { "redacts": "$target" },
			},
		}));

		assert_eq!(unsigned(&redacted), &expected);
	}

	#[test]
	fn unredacted_content_is_not_an_already_redacted() {
		let target = object(json!({
			"type": "m.room.message",
			"content": { "body": "hello", "msgtype": "m.text" },
		}));

		let applied = already_redacted(target, &rules(), because()).expect("redactable event");

		assert!(applied.is_none());
	}

	#[test]
	fn emptied_content_is_restored_alone_in_unsigned() {
		let target = object(json!({
			"type": "m.room.message",
			"content": {},
			"unsigned": { "foreign.reference": "$redaction" },
		}));

		let redacted = already_redacted(target, &rules(), because())
			.expect("redactable event")
			.expect("redaction already applied");

		let unsigned = unsigned(&redacted);

		assert_eq!(unsigned.len(), 1);
		assert!(unsigned.contains_key("redacted_because"));
	}

	#[test]
	fn kept_content_is_an_already_redacted() {
		let target = object(json!({
			"type": "m.room.member",
			"state_key": "@alice:example.org",
			"content": { "membership": "join" },
		}));

		let applied = already_redacted(target, &rules(), because()).expect("redactable event");

		assert!(applied.is_some());
	}

	#[test]
	fn restored_row_matches_a_native_redaction() {
		let original = object(json!({
			"type": "m.room.message",
			"room_id": "!room:example.org",
			"sender": "@alice:example.org",
			"content": { "body": "hello", "msgtype": "m.text" },
			"unsigned": { "transaction_id": "target-txn" },
		}));

		let imported = object(json!({
			"type": "m.room.message",
			"room_id": "!room:example.org",
			"sender": "@alice:example.org",
			"content": {},
			"unsigned": { "foreign.reference": "$redaction" },
		}));

		let native =
			redact_keeping_thread(original, true, &rules(), because()).expect("redactable event");

		let restored = already_redacted(imported, &rules(), because())
			.expect("redactable event")
			.expect("redaction already applied");

		assert_eq!(restored, native);
	}

	#[test]
	fn restored_root_keeps_its_thread_summary() {
		let relations =
			json!({ "m.thread": { "count": 2, "latest_event": { "event_id": "$reply" } } });

		let target = object(json!({
			"type": "m.room.message",
			"content": {},
			"unsigned": { "m.relations": relations },
		}));

		let redacted = already_redacted(target, &rules(), because())
			.expect("redactable event")
			.expect("redaction already applied");

		assert_eq!(
			unsigned(&redacted).get("m.relations"),
			Some(&CanonicalJsonValue::Object(object(relations)))
		);
	}

	#[test]
	fn unredactable_or_foreign_room_rows_are_left_alone() {
		let room_id = room_id!("!room:example.org");
		let unrecorded = object(json!({ "room_id": room_id, "content": {} }));
		let create = object(json!({
			"type": "m.room.create",
			"room_id": room_id,
			"content": { "room_version": "11" },
		}));

		let elsewhere = object(json!({ "room_id": "!other:example.org", "content": {} }));

		assert!(restorable(&unrecorded, room_id));
		assert!(!restorable(&create, room_id));
		assert!(!restorable(&elsewhere, room_id));
	}

	#[test]
	fn recorded_redaction_is_found() {
		let unrecorded = object(json!({ "content": {} }));
		let recorded = object(json!({
			"unsigned": { "redacted_because": { "event_id": "$redaction" } },
		}));

		let nameless = object(json!({ "unsigned": { "redacted_because": {} } }));

		assert!(recorded_redaction(&unrecorded).is_none());
		assert!(recorded_redaction(&recorded).is_some());
		assert!(recorded_redaction(&nameless).is_some());
	}

	fn object(value: serde_json::Value) -> CanonicalJsonObject {
		from_value(value).expect("canonical JSON object")
	}

	fn unsigned(pdu: &CanonicalJsonObject) -> &CanonicalJsonObject {
		pdu.get("unsigned")
			.and_then(CanonicalJsonValue::as_object)
			.expect("unsigned object")
	}

	fn rules() -> RedactionRules {
		RoomVersionId::V11
			.rules()
			.expect("known room version")
			.redaction
	}

	fn because() -> RedactedBecause {
		redacted_because(object(json!({
			"type": "m.room.redaction",
			"content": { "redacts": "$target" },
		})))
	}
}
