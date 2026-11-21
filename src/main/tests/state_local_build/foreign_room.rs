use tuwunel_core::{
	Result, async_noinline,
	ruma::{CanonicalJsonObject, RoomId, UserId},
	utils::result::NotFound,
};
use tuwunel_matrix::PduEvent;
use tuwunel_service::Services;

use super::helpers::{
	ExpectedWalkOutcome, append_message, assert_accepts, assert_fetches, assert_no_memo,
	create_room, old_held_chain, plant_memo, set_forward_extremity, sign_outlier_message,
};

// size firewall
#[async_noinline]
pub(super) async fn ancestors_fall_back<'a>(
	services: &'a Services,
	base: &'a str,
	token: &'a str,
	user_id: &'a UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let foreign = create_room(services, base, token).await?;
	let boundary = append_message(services, user_id, &room_id, "local boundary").await?;
	let foreign_boundary =
		append_message(services, user_id, &foreign, "foreign boundary").await?;

	let (PduEvent { event_id: memo, .. }, _) =
		sign_outlier_message(services, user_id, &room_id, "local memo").await?;

	let (PduEvent { event_id: foreign_memo, .. }, _) =
		sign_outlier_message(services, user_id, &foreign, "foreign memo").await?;

	let state = services
		.state
		.get_room_shortstatehash(&room_id)
		.await?;

	let foreign_state = services
		.state
		.get_room_shortstatehash(&foreign)
		.await?;

	plant_memo(services, &memo, state).await?;
	plant_memo(services, &foreign_memo, foreign_state).await?;
	set_forward_extremity(services, &foreign, &boundary).await;

	let (PduEvent { event_id: foreign_held, .. }, _) =
		sign_outlier_message(services, user_id, &foreign, "foreign held").await?;

	let frontiers = [
		(foreign_held.as_ref(), 0, Some("error"), "foreign held ancestor"),
		(foreign_boundary.as_ref(), 0, Some("error"), "foreign committed frontier"),
		(foreign_memo.as_ref(), 1, Some("error"), "foreign memoized frontier"),
		(boundary.as_ref(), 0, None, "local committed frontier"),
		(memo.as_ref(), 1, None, "local memoized frontier"),
	];

	for (frontier, memo_hits, fallback, context) in frontiers {
		let (top, top_json) = old_held_chain(services, user_id, &room_id, frontier).await?;
		let report = services
			.event_handler
			.local_state_report(&top.event_id)
			.await?;

		assert_eq!(report.fallback.as_deref(), fallback, "{context} used the wrong fallback");
		assert_eq!(report.state_len.is_some(), fallback.is_none(), "{context} built wrong state");
		assert_eq!(report.visited, 1, "{context} walked the wrong held nodes");
		assert_eq!(report.memo_hits, memo_hits, "{context} reached the wrong frontier");

		match fallback {
			| None => assert_accepts(services, &room_id, &top, top_json, context).await?,
			| Some(_) => refuses(services, &room_id, &top, top_json, context).await?,
		}
	}

	Ok(())
}

// size firewall
#[async_noinline]
async fn refuses<'a>(
	services: &'a Services,
	room_id: &'a RoomId,
	top: &'a PduEvent,
	top_json: CanonicalJsonObject,
	context: &'a str,
) -> Result {
	assert_fetches(services, room_id, top, top_json, ExpectedWalkOutcome::Error, context).await?;

	let stateless = services
		.state
		.pdu_shortstatehash(&top.event_id)
		.await
		.is_not_found();

	assert!(stateless, "{context} gave the incoming event a state");

	assert_no_memo(services, &top.event_id).await
}
