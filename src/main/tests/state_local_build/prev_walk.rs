use std::{pin::pin, time::Duration};

use futures::future::{Either, join, select};
use tokio::time::{sleep, timeout};
use tuwunel_core::{
	Err, Result, async_noinline, err,
	ruma::{EventId, RoomId, UserId},
};
use tuwunel_service::{Services, rooms::event_handler::PrevWalkMetrics};

use super::helpers::{
	SignedPdu, append_message, assert_prev_walk, create_room, redeliver, set_forward_extremity,
	sign_message, sign_outlier_message,
};

// size firewall
#[async_noinline]
pub(super) async fn prev_walk_fetch_ends<'a>(
	services: &'a Services,
	base: &'a str,
	token: &'a str,
	user_id: &'a UserId,
) -> Result {
	let closed_room = create_room(services, base, token).await?;

	gap_closed_during_fetch(services, user_id, &closed_room).await?;

	let failed_room = create_room(services, base, token).await?;
	let foreign_room = create_room(services, base, token).await?;

	foreign_prev_fails_fetch(services, user_id, &failed_room, &foreign_room).await?;

	let dropped_room = create_room(services, base, token).await?;

	dropped_fetch_is_cancelled(services, user_id, &dropped_room).await
}

// size firewall
#[async_noinline]
async fn gap_closed_during_fetch<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let context = "gap closed during the fetch";
	let ((prev, prev_json), (incoming, incoming_json)) =
		sign_gapped_pair(services, user_id, room_id).await?;

	let before = services.event_handler.prev_walk_metrics();
	let deliver = redeliver(services, room_id, &incoming, incoming_json, context);
	// Counts closed unless this append outlasts both the gap wait and the prefetch.
	let close_gap = async {
		gap_checked(services, before.gapped).await?;
		redeliver(services, room_id, &prev, prev_json, "closing prev").await
	};

	let (appended, closed) = join(deliver, close_gap).await;

	assert!(closed?, "the closing prev was not appended");
	assert!(appended?, "{context} did not append the incoming event");

	let after = services.event_handler.prev_walk_metrics();
	let expected = PrevWalkMetrics {
		entered: 2,
		gapped: 1,
		closed: 1,
		..PrevWalkMetrics::default()
	};

	assert_prev_walk(before, after, expected, context);

	Ok(())
}

// size firewall
#[async_noinline]
async fn foreign_prev_fails_fetch<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
	foreign_room: &'a RoomId,
) -> Result {
	let context = "foreign prev fails the fetch";
	let (foreign, _) =
		sign_outlier_message(services, user_id, foreign_room, "foreign prev").await?;

	let boundary = append_message(services, user_id, room_id, "foreign prev boundary").await?;
	let (incoming, incoming_json) =
		sign_child(services, user_id, room_id, &foreign.event_id, &boundary, "foreign child")
			.await?;

	let before = services.event_handler.prev_walk_metrics();
	let handled = redeliver(services, room_id, &incoming, incoming_json, context).await;

	assert!(handled.is_err(), "{context} was handled despite a prev from another room");

	let after = services.event_handler.prev_walk_metrics();
	let expected = PrevWalkMetrics {
		entered: 1,
		gapped: 1,
		fetch_failed: 1,
		..PrevWalkMetrics::default()
	};

	assert_prev_walk(before, after, expected, context);

	Ok(())
}

// size firewall
#[async_noinline]
async fn dropped_fetch_is_cancelled<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let context = "fetch dropped while waiting on the gap";
	let (_, (incoming, incoming_json)) = sign_gapped_pair(services, user_id, room_id).await?;
	let before = services.event_handler.prev_walk_metrics();
	// owned, so dropping it cancels the delivery
	let deliver = Box::pin(redeliver(services, room_id, &incoming, incoming_json, context));
	let checked = pin!(gap_checked(services, before.gapped));
	let Either::Right((checked, deliver)) = select(deliver, checked).await else {
		return Err!("{context} finished before it could be dropped");
	};

	checked?;
	drop(deliver);

	let after = services.event_handler.prev_walk_metrics();
	let expected = PrevWalkMetrics {
		entered: 1,
		gapped: 1,
		fetch_cancelled: 1,
		..PrevWalkMetrics::default()
	};

	assert_prev_walk(before, after, expected, context);

	Ok(())
}

async fn sign_gapped_pair(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<(SignedPdu, SignedPdu)> {
	let boundary = append_message(services, user_id, room_id, "gap boundary").await?;
	let prev = sign_outlier_message(services, user_id, room_id, "gap prev").await?;
	let incoming =
		sign_child(services, user_id, room_id, &prev.0.event_id, &boundary, "gap incoming").await;

	remove_outlier(services, &prev.0.event_id).await?;

	Ok((prev, incoming?))
}

/// Signs a message whose only previous event is `prev_id`, then points the room
/// back at `boundary`.
///
/// The extremity is restored before a signing error propagates, so a failed
/// case leaves the room as it found it.
async fn sign_child(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	prev_id: &EventId,
	boundary: &EventId,
	body: &str,
) -> Result<SignedPdu> {
	set_forward_extremity(services, room_id, prev_id).await;

	let signed = sign_message(services, user_id, room_id, body).await;

	set_forward_extremity(services, room_id, boundary).await;

	signed
}

/// Deletes the outlier row that let the child be signed.
///
/// Signing reads the depth of every previous event, so the prev is stored as an
/// outlier first; without the row it is missing everywhere, a gap the fetch has
/// to wait on.
async fn remove_outlier(services: &Services, event_id: &EventId) -> Result {
	let outliers = services
		.db
		.get("eventid_outlierpdu")
		.map_err(|error| err!("outlier map unavailable for {event_id}: {error}"))?;

	outliers.remove(event_id.as_str());
	services.clear_cache().await;

	let absent = !services.timeline.pdu_exists(event_id).await;

	assert!(absent, "the removed outlier {event_id} is still readable");

	Ok(())
}

async fn gap_checked(services: &Services, gapped_before: u64) -> Result {
	let checked = || services.event_handler.prev_walk_metrics().gapped > gapped_before;

	timeout(Duration::from_secs(5), async {
		while !checked() {
			sleep(Duration::from_millis(1)).await;
		}
	})
	.await
	.map_err(|_| err!("the incoming event never reached the gap check"))
}
