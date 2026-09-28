use tuwunel_core::{
	Err, Result, async_noinline,
	matrix::PduEvent,
	pdu::PduBuilder,
	ruma::{
		CanonicalJsonObject, EventId, OwnedEventId, RoomId, UserId,
		events::{
			StateEventType,
			room::member::{MembershipState, RoomMemberEventContent},
		},
	},
	utils::result::NotFound,
};
use tuwunel_service::Services;

use super::helpers::{
	SignedPdu, redeliver, set_forward_extremity, sign_outlier_message, sign_state,
};

// size firewall
#[async_noinline]
pub(super) async fn soft_failed_event_keeps_state_row<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let (first, first_json) = sign_leave(services, user_id, room_id, "first leave").await?;
	let (delayed, delayed_json) = sign_leave(services, user_id, room_id, "delayed leave").await?;
	let original_prev = match first.prev_events.as_slice() {
		| [prev] => prev,
		| [] => return Err!("first leave has no predecessor"),
		| _ => return Err!("first leave has multiple predecessors"),
	};

	let top_event_id = prepare_soft_fail_descendant(
		services,
		user_id,
		room_id,
		&delayed,
		&delayed_json,
		original_prev,
	)
	.await?;

	let accepted = redeliver(services, room_id, &first, first_json, "first leave").await?;

	assert!(accepted, "first leave was not accepted");

	let handled = redeliver(services, room_id, &delayed, delayed_json, "delayed leave").await?;

	assert!(!handled, "delayed leave was not soft failed");

	let delayed_id: &EventId = delayed.event_id.as_ref();
	let soft_failed = services
		.pdu_metadata
		.is_event_soft_failed(delayed_id)
		.await;

	assert!(soft_failed, "delayed leave lacks its soft-fail marker");

	let absent = services
		.timeline
		.non_outlier_pdu_exists(delayed_id)
		.await
		.is_not_found();

	assert!(absent, "soft-failed event reached the timeline");

	let retained = services.timeline.pdu_exists(delayed_id).await;

	assert!(retained, "soft-failed event disappeared from the outlier store");

	let shortstatehash = services
		.state
		.pdu_shortstatehash(delayed_id)
		.await?;

	let has_create = services
		.state_accessor
		.state_get_id(shortstatehash, &StateEventType::RoomCreate, "")
		.await
		.is_ok();

	assert!(has_create, "soft-fail state row has no create event");

	let has_membership = services
		.state_accessor
		.state_get_id(shortstatehash, &StateEventType::RoomMember, user_id.as_str())
		.await
		.is_ok();

	assert!(has_membership, "soft-fail state row has no positional membership");

	let report = services
		.event_handler
		.local_state_report(&top_event_id)
		.await?;

	assert_eq!(report.visited, 1, "descendant walk missed its held predecessor");
	assert_eq!(report.gate_drops, 1, "descendant walk did not fold the soft-failed predecessor");

	assert_eq!(report.fallback, None, "descendant walk fell back");
	assert!(report.state_len.is_some(), "descendant walk produced no state");

	Ok(())
}

async fn sign_leave(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	reason: &str,
) -> Result<SignedPdu> {
	let content = RoomMemberEventContent {
		reason: Some(reason.to_owned()),
		..RoomMemberEventContent::new(MembershipState::Leave)
	};

	let builder = PduBuilder::state(user_id.as_str(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}

// size firewall
#[async_noinline]
async fn prepare_soft_fail_descendant<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
	delayed: &'a PduEvent,
	delayed_json: &'a CanonicalJsonObject,
	original_prev: &'a EventId,
) -> Result<OwnedEventId> {
	services
		.timeline
		.add_pdu_outlier(&delayed.event_id, delayed_json);

	set_forward_extremity(services, room_id, delayed.event_id.as_ref()).await;

	// size firewall
	let (held, held_json) =
		Box::pin(sign_state(services, user_id, room_id, "held after leave")).await?;

	services
		.timeline
		.add_pdu_outlier(&held.event_id, &held_json);

	set_forward_extremity(services, room_id, held.event_id.as_ref()).await;

	// size firewall
	let (top, _) =
		Box::pin(sign_outlier_message(services, user_id, room_id, "top after leave")).await?;

	set_forward_extremity(services, room_id, original_prev).await;

	Ok(top.event_id)
}
