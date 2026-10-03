use futures::StreamExt;
use tuwunel_core::{
	Error, Result, err,
	ruma::{RoomId, RoomVersionId, UserId},
	utils::result::NotFound,
};
use tuwunel_matrix::pdu::into_outgoing_federation;
use tuwunel_service::{
	Services,
	rooms::event_handler::{PrevWalkMetrics, PrevWalkOutcome},
};

use super::{
	derived::sibling_state_prevs_both_resolve,
	helpers::{
		ExpectedPass, ExpectedWalkOutcome, assert_one_settled_walk, assert_prev_walk,
		assert_recorded, counter_delta, create_room, create_room_version, held_fork,
		held_state_fork, sign_message, suppress_upgrade, walk_metrics,
	},
	positional::{missing_create_falls_through_to_fetch, positional_rejection_stays_uncommitted},
	prev_walk::prev_walk_ends,
	redelivery::gapped_redelivery_backs_off,
	soft_fail::soft_failed_event_keeps_state_row,
};

pub(super) async fn enabled_baseline(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let step = |label: &'static str| move |error: Error| err!("baseline {label} failed: {error}");

	let label = "held multi-prev fork";
	let fork_room = create_room(services, base, token)
		.await
		.map_err(step(label))?;

	held_multi_prev_fork_resolves_locally(services, user_id, &fork_room)
		.await
		.map_err(step(label))?;

	let label = "v12 conflicted fork";
	let v12_fork_room = create_room_version(services, base, token, &RoomVersionId::V12)
		.await
		.map_err(step(label))?;

	held_conflicted_fork_resolves_locally(services, user_id, &v12_fork_room)
		.await
		.map_err(step(label))?;

	let label = "positional rejection";
	let denial_room = create_room(services, base, token)
		.await
		.map_err(step(label))?;

	positional_rejection_stays_uncommitted(services, user_id, &denial_room)
		.await
		.map_err(step(label))?;

	let label = "missing-create fallback";
	let missing_create_room = create_room(services, base, token)
		.await
		.map_err(step(label))?;

	missing_create_falls_through_to_fetch(services, user_id, &missing_create_room)
		.await
		.map_err(step(label))?;

	let label = "soft-failed state row";
	let soft_fail_room = create_room(services, base, token)
		.await
		.map_err(step(label))?;

	soft_failed_event_keeps_state_row(services, user_id, &soft_fail_room)
		.await
		.map_err(step(label))?;

	let label = "gapped redelivery backoff";
	let redelivery_room = create_room(services, base, token)
		.await
		.map_err(step(label))?;

	gapped_redelivery_backs_off(services, user_id, &redelivery_room)
		.await
		.map_err(step(label))?;

	let label = "sibling state prevs";
	let sibling_room = create_room(services, base, token)
		.await
		.map_err(step(label))?;

	sibling_state_prevs_both_resolve(services, user_id, &sibling_room)
		.await
		.map_err(step(label))?;

	prev_walk_ends(services, base, token, user_id)
		.await
		.map_err(step("prev walk ends"))
}

async fn held_multi_prev_fork_resolves_locally(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let (left, left_json) = sign_message(services, user_id, room_id, "left").await?;
	let (right, right_json) = sign_message(services, user_id, room_id, "right").await?;
	let (top, top_json) =
		held_fork(services, user_id, room_id, (&left, &left_json), (&right, &right_json), "top")
			.await?;

	suppress_upgrade(services, &left.event_id)?;
	suppress_upgrade(services, &right.event_id)?;

	let shortstatehash = services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	let expected_state_len = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.count()
		.await;

	let before_report = services.event_handler.state_local_metrics();

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	let after_report = services.event_handler.state_local_metrics();

	assert_eq!(after_report, before_report, "local state diagnostic changed production metrics");

	assert_eq!(report.visited, 2, "local traversal missed a held parent");
	assert_eq!(report.forks, 1, "local traversal missed the fork");
	assert_eq!(report.memo_hits, 0, "local traversal used a memo");
	assert_eq!(report.gate_drops, 0, "local traversal dropped an event");
	assert_eq!(report.fallback, None, "local traversal used federation");
	assert_eq!(
		report.state_len,
		Some(expected_state_len),
		"local resolution changed the state size"
	);

	let room_version = services.state.get_room_version(room_id).await?;
	let top_json = into_outgoing_federation(top_json, &room_version);

	let context = "held multi-prev fork";
	let before = services.event_handler.state_local_metrics();
	let walks_before = walk_metrics(services);

	services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			top.event_id.as_ref(),
			top_json,
			true,
		)
		.await?;

	let after = services.event_handler.state_local_metrics();
	let walks_after = walk_metrics(services);
	let appended = PrevWalkMetrics {
		entered: 1,
		gapped: 1,
		walked: 1,
		walked_prevs: 2,
		appended: 1,
		unprocessed_prevs: 2,
		..PrevWalkMetrics::default()
	};

	let expected = [ExpectedPass {
		event_id: &top.event_id,
		outcome: PrevWalkOutcome::Appended,
		prevs: 2,
		unprocessed: 2,
	}];

	assert_one_settled_walk(before, after, ExpectedWalkOutcome::Resolved, context);
	assert_prev_walk(walks_before, walks_after, appended, context);
	assert_recorded(services, room_id, &expected, context).await;

	let resolved = counter_delta(after.walk_resolved, before.walk_resolved, context);

	assert_eq!(resolved, 1, "held multi-prev fork did not resolve locally");

	services
		.timeline
		.non_outlier_pdu_exists(top.event_id.as_ref())
		.await?;

	for parent in [left.event_id.as_ref(), right.event_id.as_ref()] {
		let absent = services
			.timeline
			.non_outlier_pdu_exists(parent)
			.await
			.is_not_found();

		assert!(absent, "held parent unexpectedly reached the timeline");

		let stored = services.timeline.pdu_exists(parent).await;

		assert!(stored, "held parent disappeared from the outlier store");
	}

	Ok(())
}

async fn held_conflicted_fork_resolves_locally(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let (left, right, top, top_json) = held_state_fork(services, user_id, room_id).await?;

	suppress_upgrade(services, left.event_id.as_ref())?;
	suppress_upgrade(services, right.event_id.as_ref())?;

	let shortstatehash = services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	let expected_state_len = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.count()
		.await
		.checked_add(1)
		.expect("state length fits in usize");

	let before_report = services.event_handler.state_local_metrics();
	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	let after_report = services.event_handler.state_local_metrics();

	assert_eq!(after_report, before_report, "local state diagnostic changed production metrics");
	assert_eq!(report.forks, 1, "v12 local traversal missed the conflicted fork");
	assert_eq!(report.memo_hits, 0, "v12 local traversal used a memo");
	assert_eq!(report.gate_drops, 0, "v12 local traversal dropped an event");
	assert_eq!(report.fallback, None, "v12 local traversal used federation");
	assert_eq!(
		report.state_len,
		Some(expected_state_len),
		"v12 resolution did not add exactly one conflicted state key",
	);

	let room_version = services.state.get_room_version(room_id).await?;

	assert_eq!(room_version, RoomVersionId::V12, "test room changed version");

	let top_json = into_outgoing_federation(top_json, &room_version);
	let before = services.event_handler.state_local_metrics();

	services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			top.event_id.as_ref(),
			top_json,
			true,
		)
		.await?;

	let after = services.event_handler.state_local_metrics();

	assert_one_settled_walk(before, after, ExpectedWalkOutcome::Resolved, "v12 conflicted fork");
	services
		.timeline
		.non_outlier_pdu_exists(top.event_id.as_ref())
		.await?;

	for parent in [left.event_id.as_ref(), right.event_id.as_ref()] {
		let absent = services
			.timeline
			.non_outlier_pdu_exists(parent)
			.await
			.is_not_found();

		assert!(absent, "v12 held parent unexpectedly reached the timeline");
		assert!(services.timeline.pdu_exists(parent).await, "v12 held parent was lost");
	}

	Ok(())
}
