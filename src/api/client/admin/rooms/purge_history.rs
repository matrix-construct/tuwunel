use std::pin::pin;

use axum::extract::State;
use futures::StreamExt;
use ruma::{EventId, MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId, api::Direction};
use synapse_admin_api::purge_history::{
	purge::{
		by_event::{Request as PurgeByEventRequest, Response as PurgeByEventResponse},
		v1::{Request as PurgeRequest, Response as PurgeResponse},
	},
	status::v1::{PurgeStatus, Request as StatusRequest, Response as StatusResponse},
};
use tuwunel_core::{Err, Result, err, matrix::pdu::PduCount};
use tuwunel_service::tasks::Status;

use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/purge_history/{room_id}`
///
/// Schedules a background purge of the room's history strictly before the given
/// event or timestamp, returning the purge task id.
pub(crate) async fn admin_purge_history_route(
	State(services): State<crate::State>,
	body: Ruma<PurgeRequest>,
) -> Result<PurgeResponse> {
	require_admin(&services, body.sender_user()).await?;

	let boundary = resolve_boundary(
		&services,
		&body.room_id,
		body.purge_up_to_event_id.as_deref(),
		body.purge_up_to_ts,
	)
	.await?;

	let purge_id =
		schedule_purge(services, body.room_id.clone(), boundary, body.delete_local_events);

	Ok(PurgeResponse { purge_id })
}

/// # `POST /_synapse/admin/v1/purge_history/{room_id}/{event_id}`
///
/// Purge variant that takes the boundary event in the path.
pub(crate) async fn admin_purge_history_by_event_route(
	State(services): State<crate::State>,
	body: Ruma<PurgeByEventRequest>,
) -> Result<PurgeByEventResponse> {
	require_admin(&services, body.sender_user()).await?;

	let boundary = resolve_boundary(&services, &body.room_id, Some(&body.event_id), None).await?;

	let purge_id =
		schedule_purge(services, body.room_id.clone(), boundary, body.delete_local_events);

	Ok(PurgeByEventResponse { purge_id })
}

/// # `GET /_synapse/admin/v1/purge_history_status/{purge_id}`
///
/// Reports the stage of a history purge. A still-scheduled purge is reported as
/// active, matching Synapse.
pub(crate) async fn admin_purge_history_status_route(
	State(services): State<crate::State>,
	body: Ruma<StatusRequest>,
) -> Result<StatusResponse> {
	require_admin(&services, body.sender_user()).await?;

	let task = services
		.tasks
		.get(&body.purge_id)
		.filter(|task| task.action == super::PURGE_HISTORY_ACTION)
		.ok_or_else(|| err!(Request(NotFound("Unknown purge task"))))?;

	Ok(StatusResponse {
		status: purge_status(task.status),
		error: task.error,
	})
}

/// Resolves the exclusive purge boundary from the boundary event or timestamp,
/// erroring when neither is given, the event is unknown or belongs to another
/// room, or no event precedes the timestamp.
async fn resolve_boundary(
	services: &crate::State,
	room_id: &RoomId,
	event_id: Option<&EventId>,
	ts: Option<MilliSecondsSinceUnixEpoch>,
) -> Result<PduCount> {
	if let Some(event_id) = event_id {
		let pdu = services
			.timeline
			.get_pdu(event_id)
			.await
			.map_err(|_| err!(Request(NotFound("Event not found"))))?;

		if pdu.room_id != *room_id {
			return Err!(Request(BadJson("Event is for wrong room")));
		}

		return services
			.timeline
			.get_pdu_count(event_id)
			.await
			.map_err(|_| err!(Request(NotFound("Event not found"))));
	}

	let Some(ts) = ts else {
		return Err!(Request(BadJson(
			"One of purge_up_to_event_id or purge_up_to_ts must be provided"
		)));
	};

	let events = services
		.timeline
		.pdus_near_ts(None, room_id, ts, Direction::Backward);

	let mut events = pin!(events);

	events
		.next()
		.await
		.transpose()?
		.map(|(count, _)| count)
		.ok_or_else(|| err!(Request(NotFound("No event found before the given timestamp"))))
}

/// Spawns the purge on the tasks service, returning its id. Takes `services` by
/// value (it is `Copy`) so the detached task owns a `'static` handle.
fn schedule_purge(
	services: crate::State,
	room_id: OwnedRoomId,
	boundary: PduCount,
	delete_local_events: bool,
) -> String {
	let resource_id = room_id.to_string();

	let work = async move {
		let purged = services
			.timeline
			.purge_history(&room_id, boundary, delete_local_events)
			.await?;

		Ok(serde_json::json!({ "purged": purged }))
	};

	services
		.tasks
		.spawn(super::PURGE_HISTORY_ACTION, resource_id, work)
		.to_string()
}

fn purge_status(status: Status) -> PurgeStatus {
	match status {
		| Status::Scheduled | Status::Active => PurgeStatus::Active,
		| Status::Complete => PurgeStatus::Complete,
		| Status::Failed => PurgeStatus::Failed,
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;
	use tuwunel_service::tasks::Status;

	use super::purge_status;

	#[test]
	fn collapses_scheduled_into_active() {
		let status = |status| serde_json::to_value(purge_status(status)).unwrap();

		assert_eq!(status(Status::Scheduled), json!("active"));
		assert_eq!(status(Status::Active), json!("active"));
		assert_eq!(status(Status::Complete), json!("complete"));
		assert_eq!(status(Status::Failed), json!("failed"));
	}
}
