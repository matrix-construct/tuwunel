use std::iter::once;

use tuwunel_core::{
	Err, Result,
	matrix::pdu::into_outgoing_federation,
	ruma::{EventId, RoomId, UserId},
};
use tuwunel_database::Deserialized;
use tuwunel_service::{Services, rooms::short::ShortStateHash};

use super::helpers::{sign_message, suppress_upgrade};

pub(super) async fn disabled_local_build_ignores_planted_memo(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let (held, held_json) = sign_message(services, user_id, room_id, "held").await?;

	services
		.timeline
		.add_pdu_outlier(&held.event_id, &held_json);

	suppress_upgrade(services, &held.event_id)?;

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_forward_extremities(room_id, once(held.event_id.as_ref()), &state_lock)
		.await;

	drop(state_lock);

	let (incoming, incoming_json) = sign_message(services, user_id, room_id, "incoming").await?;
	let shortstatehash = services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	let resolved_state = services.db.get("eventid_resolvedstate")?;
	let incoming_event_id: &EventId = incoming.event_id.as_ref();

	resolved_state.raw_aput::<{ size_of::<ShortStateHash>() }, _, _>(
		incoming_event_id.as_bytes(),
		shortstatehash,
	);

	let planted: ShortStateHash = resolved_state
		.get(incoming_event_id)
		.await
		.deserialized()?;

	assert_eq!(planted, shortstatehash, "planted resolved-state memo did not round-trip");

	let room_version = services.state.get_room_version(room_id).await?;
	let incoming_json = into_outgoing_federation(incoming_json, &room_version);
	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			incoming_event_id,
			incoming_json,
			true,
		)
		.await;

	let Err(error) = result else {
		return Err!("disabled local build served the planted memo");
	};

	if !error
		.to_string()
		.contains("no candidate servers available")
	{
		return Err!("disabled local build failed before federation fallback: {error}");
	}

	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(incoming_event_id)
			.await
			.is_err_and(|error| error.is_not_found()),
		"incoming event unexpectedly reached the timeline"
	);
	assert!(
		services
			.timeline
			.pdu_exists(incoming_event_id)
			.await,
		"incoming event was not retained as an outlier"
	);

	Ok(())
}
