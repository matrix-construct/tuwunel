use tuwunel_core::{
	Result, err,
	ruma::{EventId, UserId},
};
use tuwunel_database::Deserialized;
use tuwunel_service::{Services, rooms::short::ShortStateHash};

use super::helpers::{
	ExpectedWalkOutcome, append_message, assert_accepts, assert_fetches, assert_no_memo,
	create_room, held_message_chain, set_forward_extremity, sign_message, suppress_upgrade,
};

pub(super) async fn direct_memo_failure_is_miss(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let boundary = append_message(services, user_id, &room_id, "direct memo boundary").await?;
	let (held, top, top_json) =
		held_message_chain(services, user_id, &room_id, &boundary).await?;

	services.clear_cache().await;
	suppress_upgrade(services, held.event_id.as_ref())?;
	plant_memo(services, top.event_id.as_ref(), ShortStateHash::MAX).await?;

	let memo = services.db.get("eventid_resolvedstate")?;

	memo.exists(&top.event_id)
		.await
		.map_err(|error| err!("direct memo fixture was not planted: {error}"))?;

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	assert_eq!(report.memo_hits, 0, "direct memo failure entered the walk");
	assert_eq!(report.gate_drops, 0, "direct memo failure became a denial");
	assert_eq!(report.fallback, None, "direct memo failure triggered fallback");
	assert!(report.state_len.is_some(), "direct memo failure produced no state");

	assert_accepts(services, &room_id, &top, top_json, "direct memo failure").await
}

pub(super) async fn walk_memo_failure_is_unevaluable(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let boundary = append_message(services, user_id, &room_id, "walk memo boundary").await?;
	let (memo, middle, _) = held_message_chain(services, user_id, &room_id, &boundary).await?;

	set_forward_extremity(services, &room_id, middle.event_id.as_ref()).await;

	let (top, top_json) = sign_message(services, user_id, &room_id, "walk memo top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	services.clear_cache().await;
	suppress_upgrade(services, memo.event_id.as_ref())?;
	suppress_upgrade(services, middle.event_id.as_ref())?;
	plant_memo(services, memo.event_id.as_ref(), ShortStateHash::MAX).await?;

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	assert_eq!(report.memo_hits, 1, "walk memo was not materialized");
	assert_eq!(report.gate_drops, 0, "walk memo failure became a denial");
	assert_eq!(
		report.fallback.as_deref(),
		Some("unevaluable"),
		"walk memo failure used the wrong fallback",
	);

	assert_eq!(report.state_len, None, "walk memo failure produced state");
	assert_no_memo(services, middle.event_id.as_ref()).await?;

	assert_fetches(
		services,
		&room_id,
		&top,
		top_json,
		ExpectedWalkOutcome::Unevaluable,
		"walk memo failure",
	)
	.await
}

async fn plant_memo(
	services: &Services,
	event_id: &EventId,
	shortstatehash: ShortStateHash,
) -> Result {
	let memo = services
		.db
		.get("eventid_resolvedstate")
		.map_err(|error| err!("resolved-state memo map unavailable for {event_id}: {error}"))?;

	memo.raw_aput::<{ size_of::<ShortStateHash>() }, _, _>(event_id.as_bytes(), shortstatehash);

	let stored: ShortStateHash = memo
		.get(event_id)
		.await
		.deserialized()
		.map_err(|error| err!("resolved-state memo for {event_id} was unreadable: {error}"))?;

	assert_eq!(
		stored, shortstatehash,
		"resolved-state memo for {event_id} stored the wrong state hash"
	);

	Ok(())
}
