use futures::TryStreamExt;
use tuwunel_core::{
	Result, err,
	ruma::{EventId, UserId},
};
use tuwunel_service::Services;

use super::helpers::{
	ExpectedWalkOutcome, append_message, append_state, assert_fetches, create_room,
	remove_short_row, restore_room_state, set_forward_extremities, sign_message,
	sign_outlier_message,
};

pub(super) async fn degree_one_state_miss(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let anchor = append_message(services, user_id, &room_id, "degree one anchor").await?;
	let intact_state = services.state.pdu_shortstatehash(&anchor).await?;

	append_state(services, user_id, &room_id, "degree one change").await?;

	let boundary = append_message(services, user_id, &room_id, "degree one boundary").await?;
	let (incoming, incoming_json) =
		sign_outlier_message(services, user_id, &room_id, "degree one top").await?;

	let corrupt_state = services
		.state
		.pdu_shortstatehash(&boundary)
		.await?;

	assert_ne!(intact_state, corrupt_state, "degree one fixture reused the intact state");
	restore_room_state(services, &room_id, intact_state, &anchor).await;
	remove_short_row(services, "shortstatehash_statediff", corrupt_state).await?;

	let restored = services
		.state
		.get_room_shortstatehash(&room_id)
		.await?;

	assert_eq!(
		restored, intact_state,
		"degree one fixture did not restore the current room state"
	);

	services
		.state_accessor
		.state_full_ids_strict(intact_state)
		.map_err(|error| err!("degree one fixture corrupted the restored state: {error}"))
		.map_ok(|_| ())
		.try_collect::<()>()
		.await?;

	assert_all_committed(services, incoming.event_id.as_ref(), "degree one state miss").await?;
	assert_fetches(
		services,
		&room_id,
		&incoming,
		incoming_json,
		ExpectedWalkOutcome::AllCommitted,
		"degree one state miss",
	)
	.await
}

pub(super) async fn sibling_state_miss(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let boundary = append_message(services, user_id, &room_id, "sibling boundary").await?;
	let boundary_state = services
		.state
		.pdu_shortstatehash(&boundary)
		.await?;

	append_state(services, user_id, &room_id, "sibling left change").await?;

	let left = append_message(services, user_id, &room_id, "sibling left").await?;

	restore_room_state(services, &room_id, boundary_state, &boundary).await;

	let right = append_message(services, user_id, &room_id, "sibling right").await?;

	set_forward_extremities(services, &room_id, [left.as_ref(), right.as_ref()]).await;

	let (incoming, incoming_json) =
		sign_message(services, user_id, &room_id, "sibling top").await?;

	let left_state = services.state.pdu_shortstatehash(&left).await?;
	let right_state = services.state.pdu_shortstatehash(&right).await?;

	assert_ne!(left_state, right_state, "sibling fixture states did not diverge");

	services
		.timeline
		.add_pdu_outlier(&incoming.event_id, &incoming_json);

	remove_short_row(services, "shortstatehash_statediff", left_state).await?;
	assert_all_committed(services, incoming.event_id.as_ref(), "sibling state miss").await?;
	assert_fetches(
		services,
		&room_id,
		&incoming,
		incoming_json,
		ExpectedWalkOutcome::AllCommitted,
		"sibling state miss",
	)
	.await
}

async fn assert_all_committed(services: &Services, event_id: &EventId, context: &str) -> Result {
	let report = services
		.event_handler
		.local_state_report(event_id)
		.await?;

	assert_eq!(report.visited, 0, "{context} unexpectedly walked a held event");
	assert_eq!(report.gate_drops, 0, "{context} became a denial");
	assert_eq!(
		report.fallback.as_deref(),
		Some("all_committed"),
		"{context} used the wrong fallback",
	);

	assert_eq!(report.state_len, None, "{context} produced state");

	Ok(())
}
