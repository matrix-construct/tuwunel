use tuwunel_core::{
	Err, Result,
	ruma::{EventId, RoomId, UserId},
	utils::{BoolExt, result::NotFound},
};
use tuwunel_database::Deserialized;
use tuwunel_matrix::pdu::into_outgoing_federation;
use tuwunel_service::{Services, rooms::short::ShortStateHash};

use super::helpers::{
	set_forward_extremity, sign_message, sign_outlier_message, suppress_upgrade,
};

pub(super) async fn ignores_planted_memo(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let (held, _) = sign_outlier_message(services, user_id, room_id, "held").await?;

	suppress_upgrade(services, &held.event_id)?;
	set_forward_extremity(services, room_id, held.event_id.as_ref()).await;

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
	let Err(error) = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			incoming_event_id,
			incoming_json,
			true,
		)
		.await
	else {
		return Err!("disabled local build served the planted memo");
	};

	if error
		.to_string()
		.contains("no candidate servers available")
		.is_false()
	{
		return Err!("disabled local build failed before federation fallback: {error}");
	}

	let absent = services
		.timeline
		.non_outlier_pdu_exists(incoming_event_id)
		.await
		.is_not_found();

	assert!(absent, "incoming event unexpectedly reached the timeline");

	let retained = services
		.timeline
		.pdu_exists(incoming_event_id)
		.await;

	assert!(retained, "incoming event was not retained as an outlier");

	Ok(())
}
