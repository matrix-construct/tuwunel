//! MSC2675: `/relations` judges its parent event, not only the room.
//!
//! A parent that is unknown, in another room or hidden from the requester is
//! refused with the one 404 the spec gives for all three, and a parent from an
//! ignored sender is named as such only once the requester may read it. A
//! stranger admitted by world-readable history receives the relations unfolded.

use std::iter::once;

use reqwest::StatusCode;
use serde_json::{Map, Value, json};
use tuwunel_core::{
	Result,
	ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, UserId, event_id},
};
use tuwunel_service::Services;

use super::{Client, chunk_ids, find, refused, room_with_history};

/// The parent's own visibility decides, under the history visibility it was
/// sent with.
///
/// A member who joined after the parent, and a stranger to a room whose
/// history is shared, are refused alike. A stranger still reads a parent sent
/// while history was world readable, and the change that opened history, along
/// with the children of each.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn requester_visibility(
	reader: &Client<'_>,
	ignored: &Client<'_>,
	visible: &Client<'_>,
	stranger: &Client<'_>,
) -> Result {
	let joined = room_with_history(visible, "joined").await?;

	ignored.act(&joined, "join").await?;

	let early = visible
		.send_text(&joined, "parent-early", "sent before the reader joined")
		.await?;

	let early_ignored = ignored
		.send_text(&joined, "parent-early-ignored", "ignored before the reader joined")
		.await?;

	reader.act(&joined, "join").await?;

	visible
		.react(&joined, "parent-early-reaction", &early, "a")
		.await?;

	let late_ignored = ignored
		.send_text(&joined, "parent-late-ignored", "ignored after the reader joined")
		.await?;

	let opened = room_with_history(visible, "world_readable").await?;
	let readable = visible
		.send_text(&opened, "parent-readable", "sent while world readable")
		.await?;

	let readable_child = visible
		.react(&opened, "parent-readable-reaction", &readable, "a")
		.await?;

	let shared = json!({ "history_visibility": "shared" });

	visible
		.put_event(&format!("rooms/{opened}/state/m.room.history_visibility"), &shared)
		.await?;

	let shared = visible
		.send_text(&opened, "parent-shared", "sent once history is shared")
		.await?;

	let not_found = StatusCode::NOT_FOUND;
	let hidden = reader.try_relations(&joined, &early, "").await?;
	let hidden = refused(hidden, not_found, "M_NOT_FOUND").await?;

	let unviewable = stranger
		.try_relations(&opened, &shared, "")
		.await?;

	let unknown = reader
		.try_relations(&joined, event_id!("$parent-unknown:localhost"), "")
		.await?;

	let elsewhere = reader
		.try_relations(&joined, &readable, "")
		.await?;

	let causes = [
		(unviewable, "a stranger to shared history"),
		(unknown, "an unknown parent"),
		(elsewhere, "a parent in another room"),
	];

	for (response, cause) in causes {
		let body = refused(response, not_found, "M_NOT_FOUND").await?;

		assert_eq!(body, hidden, "{cause} was refused unlike a hidden parent");
	}

	let early_ignored = reader
		.try_relations(&joined, &early_ignored, "")
		.await?;

	refused(early_ignored, not_found, "M_NOT_FOUND").await?;

	let late_ignored = reader
		.try_relations(&joined, &late_ignored, "")
		.await?;

	assert_eq!(late_ignored.status(), not_found, "a readable ignored parent was served");

	let late_ignored: Value = late_ignored.json().await?;
	let errcode = late_ignored["errcode"]
		.as_str()
		.unwrap_or_default();

	let error = late_ignored["error"].as_str().unwrap_or_default();

	// An M_UNKNOWN response can carry the MSC4406 code in its message.
	assert!(
		matches!(errcode, "M_SENDER_IGNORED" | "UK.TIMEDOUT.MSC4406.SENDER_IGNORED")
			|| (errcode == "M_UNKNOWN" && error.contains("M_SENDER_IGNORED")),
		"a readable ignored parent was not refused as ignored: {late_ignored}"
	);

	let served = stranger.relations(&opened, &readable, "").await?;

	assert_eq!(chunk_ids(&served)?, [readable_child.as_str()], "a readable parent was withheld");

	let reopened = room_with_history(visible, "joined").await?;
	let readable = json!({ "history_visibility": "world_readable" });
	let reopening = visible
		.put_event(&format!("rooms/{reopened}/state/m.room.history_visibility"), &readable)
		.await?;

	let reopening_child = visible
		.react(&reopened, "parent-reopening-reaction", &reopening, "a")
		.await?;

	let served = stranger
		.relations(&reopened, &reopening, "")
		.await?;

	assert_eq!(
		chunk_ids(&served)?,
		[reopening_child.as_str()],
		"a change opening history was withheld from a stranger",
	);

	Ok(())
}

/// An invitation lets the requester read what was sent during it, and a forget
/// withdraws what membership let them read.
///
/// Once history narrows to joined members, an invitee, first-time or returning,
/// still reads a parent sent during their invitation, with its child, until they
/// reject the invitation and forget the room. After a former member forgets the
/// room, requesting a parent sent while history was world-readable returns not
/// found.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn membership_visibility(
	visible: &Client<'_>,
	stranger: &Client<'_>,
	stranger_id: &UserId,
) -> Result {
	let not_found = StatusCode::NOT_FOUND;
	let invite_room = room_with_history(visible, "invited").await?;

	visible
		.act_on(&invite_room, "invite", stranger_id)
		.await?;

	let invited = visible
		.send_text(&invite_room, "parent-invited", "sent while the stranger was invited")
		.await?;

	let invited_child = visible
		.react(&invite_room, "parent-invited-reaction", &invited, "a")
		.await?;

	let narrowed = json!({ "history_visibility": "joined" });

	visible
		.put_event(&format!("rooms/{invite_room}/state/m.room.history_visibility"), &narrowed)
		.await?;

	let served = stranger
		.relations(&invite_room, &invited, "")
		.await?;

	assert_eq!(
		chunk_ids(&served)?,
		[invited_child.as_str()],
		"an invitee lost a parent sent during their invitation",
	);

	stranger.act(&invite_room, "leave").await?;
	stranger.act(&invite_room, "forget").await?;

	let forgotten = stranger
		.try_relations(&invite_room, &invited, "")
		.await?;

	refused(forgotten, not_found, "M_NOT_FOUND").await?;

	let closed = room_with_history(visible, "world_readable").await?;

	stranger.act(&closed, "join").await?;

	let public_root = visible
		.send_text(&closed, "parent-public", "sent while world readable")
		.await?;

	let closing = json!({ "history_visibility": "shared" });

	visible
		.put_event(&format!("rooms/{closed}/state/m.room.history_visibility"), &closing)
		.await?;

	visible
		.react(&closed, "parent-private-reaction", &public_root, "a")
		.await?;

	stranger.act(&closed, "leave").await?;
	stranger.act(&closed, "forget").await?;

	let forgotten = stranger
		.try_relations(&closed, &public_root, "")
		.await?;

	refused(forgotten, not_found, "M_NOT_FOUND").await?;

	let return_room = room_with_history(visible, "invited").await?;

	stranger.act(&return_room, "join").await?;
	stranger.act(&return_room, "leave").await?;
	visible
		.act_on(&return_room, "invite", stranger_id)
		.await?;

	let returning = visible
		.send_text(&return_room, "parent-returning", "sent while invited after leaving")
		.await?;

	let returning_child = visible
		.react(&return_room, "parent-returning-reaction", &returning, "a")
		.await?;

	visible
		.put_event(&format!("rooms/{return_room}/state/m.room.history_visibility"), &narrowed)
		.await?;

	let served = stranger
		.relations(&return_room, &returning, "")
		.await?;

	assert_eq!(
		chunk_ids(&served)?,
		[returning_child.as_str()],
		"a returning invitee lost a parent sent during their invitation",
	);

	Ok(())
}

/// A stranger reads each relation unfolded, and pruned when its sender is
/// erased.
///
/// The stranger receives a reference sent while history was world readable
/// without the thread summary stored on it. A reaction from a sender erased
/// since then arrives with empty content.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn unfolded_view(
	visible: &Client<'_>,
	erased: &Client<'_>,
	erased_id: &UserId,
	stranger: &Client<'_>,
) -> Result {
	let opened = room_with_history(visible, "world_readable").await?;

	erased.act(&opened, "join").await?;

	let parent = visible
		.send_text(&opened, "parent-unfolded", "sent while world readable")
		.await?;

	let reference = json!({
		"msgtype": "m.text",
		"body": "a reference to the parent",
		"m.relates_to": { "rel_type": "m.reference", "event_id": parent },
	});

	let reference = visible
		.put_event(
			&format!("rooms/{opened}/send/m.room.message/parent-unfolded-reference"),
			&reference,
		)
		.await?;

	let reaction = erased
		.react(&opened, "parent-unfolded-reaction", &parent, "a")
		.await?;

	let narrowed = json!({ "history_visibility": "joined" });

	visible
		.put_event(&format!("rooms/{opened}/state/m.room.history_visibility"), &narrowed)
		.await?;

	store_thread_summary(visible.services, &reference).await?;

	erased
		.services
		.deactivate
		.full_deactivate(erased_id, true)
		.await?;

	let served = stranger.relations(&opened, &parent, "").await?;
	let chunk = &served["chunk"];

	assert!(
		find(chunk, &reference)?["unsigned"]["m.relations"].is_null(),
		"a stranger received the thread summary stored on a relation: {served}"
	);

	assert!(
		find(chunk, &reaction)?["content"]
			.as_object()
			.is_some_and(Map::is_empty),
		"a stranger received an erased sender's reaction unpruned: {served}"
	);

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn store_thread_summary(services: &Services, event_id: &EventId) -> Result {
	let pdu_id = services.timeline.get_pdu_id(event_id).await?;
	let summary = json!({
		"m.relations": { "m.thread": {
			"count": 1,
			"current_user_participated": false,
			"latest_event": { "type": "m.room.message", "content": { "body": "members only" } },
		}},
	});

	let summary: CanonicalJsonValue = serde_json::from_value(summary)?;

	let pdu: CanonicalJsonObject = services
		.timeline
		.get_pdu_json_from_id(&pdu_id)
		.await?
		.into_iter()
		.filter(|(key, _)| key != "unsigned")
		.chain(once(("unsigned".into(), summary)))
		.collect();

	services.timeline.replace_pdu(&pdu_id, &pdu).await
}
