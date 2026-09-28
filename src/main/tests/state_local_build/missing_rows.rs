use futures::TryStreamExt;
use tuwunel_core::{
	Result, err,
	ruma::{UserId, events::StateEventType},
};
use tuwunel_service::Services;

use super::helpers::{
	CacheHandling, ExpectedWalkOutcome, PduFailure, append_message, append_state, assert_fetches,
	assert_no_memo, assert_unevaluable, corrupt_timeline_pdu, create_room, held_message_chain,
	held_state_fork, remove_short_row, restore_room_state, set_forward_extremities,
	set_forward_extremity, sign_outlier_message, suppress_upgrade,
};

pub(super) async fn missing_state_diff(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let anchor = append_message(services, user_id, &room_id, "state diff anchor").await?;
	let intact_state = services.state.pdu_shortstatehash(&anchor).await?;

	append_state(services, user_id, &room_id, "state diff change").await?;

	let boundary = append_message(services, user_id, &room_id, "state diff boundary").await?;
	let (held, top, top_json) =
		held_message_chain(services, user_id, &room_id, &boundary).await?;

	let corrupt_state = services
		.state
		.pdu_shortstatehash(&boundary)
		.await?;

	assert_ne!(intact_state, corrupt_state, "state diff fixture reused the intact state");
	restore_room_state(services, &room_id, intact_state, &anchor).await;
	remove_short_row(services, "shortstatehash_statediff", corrupt_state).await?;

	let restored = services
		.state
		.get_room_shortstatehash(&room_id)
		.await?;

	assert_eq!(
		restored, intact_state,
		"state diff fixture did not restore the current room state"
	);

	services
		.state_accessor
		.state_full_ids_strict(intact_state)
		.map_err(|error| err!("state diff fixture corrupted the restored state: {error}"))
		.map_ok(|_| ())
		.try_collect::<()>()
		.await?;

	suppress_upgrade(services, held.event_id.as_ref())?;
	assert_unevaluable(services, top.event_id.as_ref(), "missing state diff").await?;
	assert_fetches(
		services,
		&room_id,
		&top,
		top_json,
		ExpectedWalkOutcome::Unevaluable,
		"missing state diff",
	)
	.await
}

pub(super) async fn missing_event_reverse(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let named = append_state(services, user_id, &room_id, "reverse mapping state").await?;
	let boundary =
		append_message(services, user_id, &room_id, "reverse mapping boundary").await?;

	let (held, top, top_json) =
		held_message_chain(services, user_id, &room_id, &boundary).await?;

	let shorteventid = services.short.get_shorteventid(&named).await?;

	remove_short_row(services, "shorteventid_eventid", shorteventid).await?;
	suppress_upgrade(services, held.event_id.as_ref())?;
	assert_unevaluable(services, top.event_id.as_ref(), "missing event reverse map").await?;
	assert_fetches(
		services,
		&room_id,
		&top,
		top_json,
		ExpectedWalkOutcome::Unevaluable,
		"missing event reverse map",
	)
	.await
}

pub(super) async fn missing_state_key_reverse(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;

	append_state(services, user_id, &room_id, "state key boundary").await?;

	let (left, right, top, top_json) = held_state_fork(services, user_id, &room_id).await?;
	let shortstatekey = services
		.short
		.get_shortstatekey(&StateEventType::RoomName, "")
		.await?;

	remove_short_row(services, "shortstatekey_statekey", shortstatekey).await?;
	suppress_upgrade(services, left.event_id.as_ref())?;
	suppress_upgrade(services, right.event_id.as_ref())?;
	assert_unevaluable(services, top.event_id.as_ref(), "missing state key reverse map").await?;
	assert_fetches(
		services,
		&room_id,
		&top,
		top_json,
		ExpectedWalkOutcome::Unevaluable,
		"missing state key reverse map",
	)
	.await
}

pub(super) async fn missing_named_pdu(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let missing = append_state(services, user_id, &room_id, "named pdu left").await?;
	let left = append_message(services, user_id, &room_id, "named pdu left boundary").await?;

	append_state(services, user_id, &room_id, "named pdu right").await?;

	let right = append_message(services, user_id, &room_id, "named pdu right boundary").await?;

	set_forward_extremities(services, &room_id, [left.as_ref(), right.as_ref()]).await;

	let (fork, _) = sign_outlier_message(services, user_id, &room_id, "named pdu fork").await?;

	set_forward_extremity(services, &room_id, fork.event_id.as_ref()).await;

	let (top, top_json) =
		sign_outlier_message(services, user_id, &room_id, "named pdu top").await?;

	corrupt_timeline_pdu(services, &missing, PduFailure::Missing, CacheHandling::Clear).await?;
	suppress_upgrade(services, fork.event_id.as_ref())?;

	let before_report = services.event_handler.state_local_metrics();

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	let after_report = services.event_handler.state_local_metrics();

	assert_eq!(after_report, before_report, "local state diagnostic changed production metrics");

	assert_eq!(report.gate_drops, 0, "missing map-named PDU became a denial");
	assert_eq!(
		report.fallback.as_deref(),
		Some("unevaluable"),
		"missing map-named PDU used the wrong fallback",
	);

	assert_eq!(report.state_len, None, "missing map-named PDU produced state");
	assert_no_memo(services, fork.event_id.as_ref()).await?;
	assert_fetches(
		services,
		&room_id,
		&top,
		top_json,
		ExpectedWalkOutcome::Unevaluable,
		"missing map-named PDU",
	)
	.await
}
