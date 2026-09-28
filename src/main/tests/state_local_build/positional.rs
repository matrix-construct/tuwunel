use std::{iter::once, sync::Arc};

use futures::{StreamExt, future::ready};
use tuwunel_core::{
	Err, Error, Result,
	matrix::pdu::into_outgoing_federation,
	ruma::{EventId, RoomId, UserId, events::StateEventType},
};
use tuwunel_service::{Services, rooms::state_compressor::CompressedState};

use super::helpers::{
	ExpectedWalkOutcome, append_message, assert_one_settled_walk, set_forward_extremities,
	sign_message, sign_state, suppress_upgrade,
};

pub(super) async fn positional_rejection_stays_uncommitted(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let base = append_message(services, user_id, room_id, "position base").await?;
	let (denied_left, denied_left_json) =
		sign_state(services, user_id, room_id, "denied left").await?;

	let (denied_right, denied_right_json) =
		sign_state(services, user_id, room_id, "denied right").await?;

	replace_state_before_without(
		services,
		room_id,
		&base,
		&StateEventType::RoomMember,
		user_id.as_str(),
	)
	.await?;

	let room_version = services.state.get_room_version(room_id).await?;

	for (denied, denied_json) in
		[(&denied_left, denied_left_json), (&denied_right, denied_right_json)]
	{
		let denied_json = into_outgoing_federation(denied_json, &room_version);
		let result = services
			.event_handler
			.handle_incoming_pdu(
				services.globals.server_name(),
				room_id,
				denied.event_id.as_ref(),
				denied_json,
				true,
			)
			.await;

		assert!(
			matches!(&result, Err(Error::AuthCheck(..))),
			"positionally invalid event had an unexpected result: {result:?}"
		);

		assert!(
			services
				.timeline
				.pdu_exists(denied.event_id.as_ref())
				.await,
			"positionally rejected event was not retained as an outlier"
		);

		assert!(
			services
				.state
				.pdu_shortstatehash(denied.event_id.as_ref())
				.await
				.is_err_and(|error| error.is_not_found()),
			"positionally rejected event gained a state row"
		);

		suppress_upgrade(services, denied.event_id.as_ref())?;
	}

	set_forward_extremities(services, room_id, [
		denied_left.event_id.as_ref(),
		denied_right.event_id.as_ref(),
	])
	.await;

	let (top, top_json) = sign_message(services, user_id, room_id, "denial top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	let before_report = services.event_handler.state_local_metrics();

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	let after_report = services.event_handler.state_local_metrics();

	assert_eq!(after_report, before_report, "local state diagnostic changed production metrics");

	assert_eq!(report.visited, 2, "local traversal missed a denied event");
	assert_eq!(report.forks, 1, "local traversal missed the denied fork");
	assert_eq!(report.gate_drops, 2, "gate denials were not counted exactly once each");
	assert_eq!(report.fallback, None, "clean gate denial triggered a fetch");
	assert!(report.state_len.is_some(), "clean gate denial lost the built state");

	let before = services.event_handler.state_local_metrics();
	let top_json = into_outgoing_federation(top_json, &room_version);
	let is_timeline_event = true;

	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			top.event_id.as_ref(),
			top_json,
			is_timeline_event,
		)
		.await;

	assert!(
		matches!(&result, Err(Error::AuthCheck(..))),
		"event over denied membership had an unexpected result: {result:?}",
	);

	let after = services.event_handler.state_local_metrics();

	assert_one_settled_walk(before, after, ExpectedWalkOutcome::Resolved, "clean gate denial");
	assert_eq!(
		after
			.walk_resolved
			.checked_sub(before.walk_resolved)
			.expect("walk resolved counter should not decrease"),
		1,
		"clean gate denial did not resolve locally",
	);

	assert_eq!(
		after
			.gate_denials
			.checked_sub(before.gate_denials)
			.expect("gate denial counter should not decrease"),
		u64::try_from(report.gate_drops).expect("gate denial count should fit in u64"),
		"clean gate denials were not aggregated exactly once each",
	);

	Ok(())
}

pub(super) async fn missing_create_falls_through_to_fetch(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let base = append_message(services, user_id, room_id, "missing create base").await?;
	let (held, held_json) = sign_state(services, user_id, room_id, "missing create").await?;

	replace_state_before_without(services, room_id, &base, &StateEventType::RoomCreate, "")
		.await?;

	services
		.timeline
		.add_pdu_outlier(&held.event_id, &held_json);

	suppress_upgrade(services, held.event_id.as_ref())?;

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_forward_extremities(room_id, once(held.event_id.as_ref()), &state_lock)
		.await;

	drop(state_lock);

	let (top, top_json) = sign_message(services, user_id, room_id, "missing create top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	assert_eq!(report.gate_drops, 0, "missing create was counted as a denial");
	assert_eq!(
		report.fallback.as_deref(),
		Some("unevaluable"),
		"missing create used the wrong fallback"
	);

	assert_eq!(report.state_len, None, "missing create produced a state");

	let room_version = services.state.get_room_version(room_id).await?;
	let top_json = into_outgoing_federation(top_json, &room_version);
	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			top.event_id.as_ref(),
			top_json,
			true,
		)
		.await;

	let Err(error) = result else {
		return Err!("missing create did not fall through to federation fetch");
	};

	assert!(
		error
			.to_string()
			.contains("no candidate servers available"),
		"missing create failed before federation fetch: {error}"
	);

	Ok(())
}

async fn replace_state_before_without(
	services: &Services,
	room_id: &RoomId,
	event_id: &EventId,
	event_type: &StateEventType,
	state_key: &str,
) -> Result {
	let shortstatehash = services
		.state
		.pdu_shortstatehash(event_id)
		.await?;

	let shortstatekey = services
		.short
		.get_shortstatekey(event_type, state_key)
		.await?;

	let (state, excluded) = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.fold((Vec::new(), false), |(mut state, mut excluded), entry| {
			if entry.0 == shortstatekey {
				excluded = true;
			} else {
				state.push(entry);
			}

			ready((state, excluded))
		})
		.await;

	if !excluded {
		return Err!("state-before fixture lacks the selected key");
	}

	let compressed: CompressedState = services
		.state_compressor
		.compress_state_events(
			state
				.iter()
				.map(|(shortstatekey, event_id)| (shortstatekey, event_id.as_ref())),
		)
		.collect()
		.await;

	let compressed = Arc::new(compressed);

	services
		.state
		.set_event_state(event_id, room_id, compressed)
		.await?;

	Ok(())
}
