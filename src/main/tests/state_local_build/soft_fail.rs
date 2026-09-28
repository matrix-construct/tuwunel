use std::collections::BTreeSet;

use futures::StreamExt;
use tuwunel_core::{
	Err, Result, async_noinline, err,
	matrix::{PduEvent, pdu::into_outgoing_federation},
	pdu::PduBuilder,
	ruma::{
		CanonicalJsonObject, EventId, OwnedEventId, RoomId, UserId,
		events::{
			StateEventType,
			room::member::{MembershipState, RoomMemberEventContent},
		},
	},
};
use tuwunel_service::Services;

use super::helpers::{set_forward_extremity, sign_message, sign_state};

#[async_noinline]
pub(super) async fn soft_failed_event_keeps_state_row<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let (first, first_json) = sign_leave(services, user_id, room_id, "first leave").await?;
	let (delayed, delayed_json) = sign_leave(services, user_id, room_id, "delayed leave").await?;
	let mut original_prevs = first.prev_events.iter();
	let original_prev = original_prevs
		.next()
		.ok_or_else(|| err!("first leave has no predecessor"))?
		.to_owned();

	if original_prevs.next().is_some() {
		return Err!("first leave has multiple predecessors");
	}

	let top_event_id = prepare_soft_fail_descendant(
		services,
		user_id,
		room_id,
		&delayed,
		&delayed_json,
		&original_prev,
	)
	.await?;

	let room_version = services.state.get_room_version(room_id).await?;
	let first_json = into_outgoing_federation(first_json, &room_version);
	let first_result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			first.event_id.as_ref(),
			first_json,
			true,
		)
		.await?;

	assert!(first_result.is_some(), "first leave was not accepted");

	let delayed_json = into_outgoing_federation(delayed_json, &room_version);
	let delayed_result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			delayed.event_id.as_ref(),
			delayed_json,
			true,
		)
		.await?;

	assert_eq!(delayed_result, None, "delayed leave was not soft failed");
	assert!(
		services
			.pdu_metadata
			.is_event_soft_failed(delayed.event_id.as_ref())
			.await,
		"delayed leave lacks its soft-fail marker"
	);

	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(delayed.event_id.as_ref())
			.await
			.is_err_and(|error| error.is_not_found()),
		"soft-failed event reached the timeline"
	);

	assert!(
		services
			.timeline
			.pdu_exists(delayed.event_id.as_ref())
			.await,
		"soft-failed event disappeared from the outlier store"
	);

	let shortstatehash = services
		.state
		.pdu_shortstatehash(delayed.event_id.as_ref())
		.await?;

	let state_keys = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.map(|(shortstatekey, _)| shortstatekey)
		.collect::<BTreeSet<_>>()
		.await;

	let create = services
		.short
		.get_shortstatekey(&StateEventType::RoomCreate, "")
		.await?;

	let membership = services
		.short
		.get_shortstatekey(&StateEventType::RoomMember, user_id.as_str())
		.await?;

	assert!(state_keys.contains(&create), "soft-fail state row has no create event");
	assert!(
		state_keys.contains(&membership),
		"soft-fail state row has no positional membership"
	);

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
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let content = RoomMemberEventContent {
		reason: Some(reason.to_owned()),
		..RoomMemberEventContent::new(MembershipState::Leave)
	};

	let builder = PduBuilder::state(user_id.to_string(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}

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

	let (held, held_json) =
		Box::pin(sign_state(services, user_id, room_id, "held after leave")).await?;

	services
		.timeline
		.add_pdu_outlier(&held.event_id, &held_json);

	set_forward_extremity(services, room_id, held.event_id.as_ref()).await;

	let (top, top_json) =
		Box::pin(sign_message(services, user_id, room_id, "top after leave")).await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	set_forward_extremity(services, room_id, original_prev).await;

	Ok(top.event_id)
}
