use tuwunel_core::{
	Result,
	matrix::PduEvent,
	ruma::{CanonicalJsonObject, EventId, RoomId, UserId},
	utils::time::now_secs,
};
use tuwunel_database::Interfix;
use tuwunel_service::Services;

use super::helpers::{
	Context, Disposition, append_message, assert_accepts, held_message_chain, plant_backoff_row,
	redeliver, sign_message,
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

	plant_incoming_rows(services, &top.event_id, Disposition::Pending, 3)?;
	plant_incoming_rows(services, failed_id, Disposition::Transient, 1)?;
	plant_incoming_rows(services, control_id, Disposition::Pending, 2)?;
	assert_backs_off(services, room_id, &top, top_json, 3, "three-attempt redelivery").await?;
	assert_backs_off(services, room_id, &failed, failed_json, 1, "failed redelivery").await?;
	assert_accepts(services, room_id, &control, control_json, "two-attempt redelivery").await?;

	let rows = incoming_rows(services, control_id).await?;

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

	plant_incoming_rows(services, closed_id, Disposition::Pending, 3)?;
	assert_accepts(services, room_id, &closed, closed_json, "closed-gap redelivery").await?;

	let rows = incoming_rows(services, closed_id).await?;

	assert_eq!(rows, 3, "closed-gap redelivery consulted the backoff store");

	Ok(())
}

fn plant_incoming_rows(
	services: &Services,
	event_id: &EventId,
	disposition: Disposition,
	rows: u32,
) -> Result {
	let now = now_secs();
	let minute = u32::try_from(now / 60)?;

	(1..=rows)
		.map(|age| minute.saturating_sub(age))
		.try_for_each(|bucket| {
			plant_backoff_row(services, Context::Incoming, event_id, bucket, disposition, now)
		})
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

	let surviving = incoming_rows(services, incoming_id).await?;

	assert_eq!(surviving, rows, "{context} touched its backoff rows");

	let timeline_row = services
		.timeline
		.non_outlier_pdu_exists(incoming_id)
		.await;

	assert!(
		timeline_row.is_err_and(|error| error.is_not_found()),
		"{context} reached the timeline"
	);

	Ok(())
}

async fn incoming_rows(services: &Services, event_id: &EventId) -> Result<usize> {
	let rows = services
		.db
		.get("eventid_backoff")?
		.count_prefix(&(u8::from(Context::Incoming), event_id, Interfix))
		.await;

	Ok(rows)
}
