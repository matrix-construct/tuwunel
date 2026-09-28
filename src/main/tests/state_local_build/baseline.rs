use futures::StreamExt;
use tuwunel_core::{
	Error, Result, err,
	matrix::pdu::into_outgoing_federation,
	ruma::{RoomId, RoomVersionId, UserId},
};
use tuwunel_service::Services;

use super::{
	helpers::{
		ExpectedWalkOutcome, assert_one_settled_walk, create_room, create_room_version,
		held_state_fork, sign_message, suppress_upgrade,
	},
	positional::{missing_create_falls_through_to_fetch, positional_rejection_stays_uncommitted},
	redelivery::gapped_redelivery_backs_off,
	soft_fail::soft_failed_event_keeps_state_row,
};

pub(super) async fn enabled_baseline(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let step_error = |step: &str, error: Error| err!("baseline {step} failed: {error}");

	let fork_room = create_room(services, base, token)
		.await
		.map_err(|error| step_error("held multi-prev fork", error))?;

	held_multi_prev_fork_resolves_locally(services, user_id, &fork_room)
		.await
		.map_err(|error| step_error("held multi-prev fork", error))?;

	let v12_fork_room = create_room_version(services, base, token, &RoomVersionId::V12)
		.await
		.map_err(|error| step_error("v12 conflicted fork", error))?;

	held_conflicted_fork_resolves_locally(services, user_id, &v12_fork_room)
		.await
		.map_err(|error| step_error("v12 conflicted fork", error))?;

	let denial_room = create_room(services, base, token)
		.await
		.map_err(|error| step_error("positional rejection", error))?;

	positional_rejection_stays_uncommitted(services, user_id, &denial_room)
		.await
		.map_err(|error| step_error("positional rejection", error))?;

	let missing_create_room = create_room(services, base, token)
		.await
		.map_err(|error| step_error("missing-create fallback", error))?;

	missing_create_falls_through_to_fetch(services, user_id, &missing_create_room)
		.await
		.map_err(|error| step_error("missing-create fallback", error))?;

	let soft_fail_room = create_room(services, base, token)
		.await
		.map_err(|error| step_error("soft-failed state row", error))?;

	soft_failed_event_keeps_state_row(services, user_id, &soft_fail_room)
		.await
		.map_err(|error| step_error("soft-failed state row", error))?;

	let redelivery_room = create_room(services, base, token)
		.await
		.map_err(|error| step_error("gapped redelivery backoff", error))?;

	gapped_redelivery_backs_off(services, user_id, &redelivery_room)
		.await
		.map_err(|error| step_error("gapped redelivery backoff", error))
}

async fn held_multi_prev_fork_resolves_locally(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let (left, left_json) = sign_message(services, user_id, room_id, "left").await?;
	let (right, right_json) = sign_message(services, user_id, room_id, "right").await?;

	services
		.timeline
		.add_pdu_outlier(&left.event_id, &left_json);

	services
		.timeline
		.add_pdu_outlier(&right.event_id, &right_json);

	suppress_upgrade(services, &left.event_id)?;
	suppress_upgrade(services, &right.event_id)?;

	let state_lock = services.state.mutex.lock(room_id).await;
	let prevs = [left.event_id.as_ref(), right.event_id.as_ref()];

	services
		.state
		.set_forward_extremities(room_id, prevs.into_iter(), &state_lock)
		.await;

	drop(state_lock);

	let (top, top_json) = sign_message(services, user_id, room_id, "top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

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

	assert_one_settled_walk(before, after, ExpectedWalkOutcome::Resolved, "held multi-prev fork");
	assert_eq!(
		after
			.walk_resolved
			.checked_sub(before.walk_resolved)
			.expect("walk resolved counter should not decrease"),
		1,
		"held multi-prev fork did not resolve locally",
	);

	services
		.timeline
		.non_outlier_pdu_exists(top.event_id.as_ref())
		.await?;

	for parent in [left.event_id.as_ref(), right.event_id.as_ref()] {
		assert!(
			services
				.timeline
				.non_outlier_pdu_exists(parent)
				.await
				.is_err_and(|error| error.is_not_found()),
			"held parent unexpectedly reached the timeline"
		);
		assert!(
			services.timeline.pdu_exists(parent).await,
			"held parent disappeared from the outlier store"
		);
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
		.await;

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
		Some(
			expected_state_len
				.checked_add(1)
				.expect("state length fits in usize")
		),
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
		assert!(
			services
				.timeline
				.non_outlier_pdu_exists(parent)
				.await
				.is_err_and(|error| error.is_not_found()),
			"v12 held parent unexpectedly reached the timeline",
		);

		assert!(services.timeline.pdu_exists(parent).await, "v12 held parent was lost");
	}

	Ok(())
}
