use tuwunel_core::{
	Result,
	matrix::PduEvent,
	ruma::{CanonicalJsonObject, EventId, RoomId, UserId},
	utils::result::NotFound,
};
use tuwunel_service::{Services, rooms::event_handler::PrevWalkMetrics};

use super::helpers::{
	Context, Disposition, append_message, assert_accepts, assert_prev_walk, backoff_rows,
	held_message_chain, plant_backoff_rows, redeliver, sign_message,
};

pub(super) async fn gapped_redelivery_backs_off(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result {
	let boundary = append_message(services, user_id, room_id, "backoff boundary").await?;
	let (_, top, top_json) = held_message_chain(services, user_id, room_id, &boundary).await?;
	let (failed, failed_json) =
		sign_message(services, user_id, room_id, "backoff failed").await?;

	let (control, control_json) =
		sign_message(services, user_id, room_id, "backoff control").await?;

	let failed_id: &EventId = failed.event_id.as_ref();
	let control_id: &EventId = control.event_id.as_ref();

	services
		.timeline
		.add_pdu_outlier(failed_id, &failed_json);

	services
		.timeline
		.add_pdu_outlier(control_id, &control_json);

	plant_backoff_rows(services, Context::Incoming, &top.event_id, Disposition::Pending, 3)?;
	plant_backoff_rows(services, Context::Incoming, failed_id, Disposition::Transient, 1)?;
	plant_backoff_rows(services, Context::Incoming, control_id, Disposition::Pending, 2)?;

	let context = "three-attempt redelivery";
	let prev_walks_before = services.event_handler.prev_walk_metrics();

	assert_backs_off(services, room_id, &top, top_json, 3, context).await?;

	let prev_walks_after = services.event_handler.prev_walk_metrics();
	let hold = PrevWalkMetrics {
		entered: 1,
		gapped: 1,
		held: 1,
		..PrevWalkMetrics::default()
	};

	assert_prev_walk(prev_walks_before, prev_walks_after, hold, context);
	assert_backs_off(services, room_id, &failed, failed_json, 1, "failed redelivery").await?;
	assert_accepts(services, room_id, &control, control_json, "two-attempt redelivery").await?;

	let rows = backoff_rows(services, Context::Incoming, control_id).await?;

	assert_eq!(rows, 0, "integrated redelivery kept its attempt rows");

	let (closed, closed_json) =
		sign_message(services, user_id, room_id, "backoff closed").await?;

	let closed_id: &EventId = closed.event_id.as_ref();

	assert_ne!(
		closed.prev_events, top.prev_events,
		"closed-gap event still names the held prev"
	);

	services
		.timeline
		.add_pdu_outlier(closed_id, &closed_json);

	plant_backoff_rows(services, Context::Incoming, closed_id, Disposition::Pending, 3)?;

	let context = "closed-gap redelivery";
	let prev_walks_before = services.event_handler.prev_walk_metrics();

	assert_accepts(services, room_id, &closed, closed_json, context).await?;

	let prev_walks_after = services.event_handler.prev_walk_metrics();
	let ungapped = PrevWalkMetrics { entered: 1, ..PrevWalkMetrics::default() };

	assert_prev_walk(prev_walks_before, prev_walks_after, ungapped, context);

	let rows = backoff_rows(services, Context::Incoming, closed_id).await?;

	assert_eq!(rows, 3, "closed-gap redelivery consulted the backoff store");

	Ok(())
}

async fn assert_backs_off(
	services: &Services,
	room_id: &RoomId,
	incoming: &PduEvent,
	incoming_json: CanonicalJsonObject,
	rows: usize,
	context: &str,
) -> Result {
	let incoming_id: &EventId = incoming.event_id.as_ref();
	let handled = redeliver(services, room_id, incoming, incoming_json, context).await?;

	assert!(!handled, "{context} was not backed off");

	let surviving = backoff_rows(services, Context::Incoming, incoming_id).await?;

	assert_eq!(surviving, rows, "{context} touched its backoff rows");

	let timeline_row = services
		.timeline
		.non_outlier_pdu_exists(incoming_id)
		.await;

	assert!(timeline_row.is_not_found(), "{context} reached the timeline");

	Ok(())
}
