use axum::extract::State;
use ruma::RoomId;
use synapse_admin_api::rooms::delete_status::{
	by_delete_id::{
		DeleteStatus, DeleteStatusKind, Request as ByDeleteIdRequest,
		Response as ByDeleteIdResponse,
	},
	by_room_id::{Request as ByRoomIdRequest, Response as ByRoomIdResponse},
};
use tuwunel_core::{Err, Result, err};
use tuwunel_service::tasks::{Status, TaskInfo};

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v2/rooms/delete_status/{delete_id}`
///
/// Reports the stage of an asynchronous room deletion by its task id, including
/// the shutdown outcome once the task has finished.
pub(crate) async fn admin_delete_status_by_id_route(
	State(services): State<crate::State>,
	body: Ruma<ByDeleteIdRequest>,
) -> Result<ByDeleteIdResponse> {
	require_admin(&services, body.sender_user()).await?;

	let task = services
		.tasks
		.get(&body.delete_id)
		.filter(|task| task.action == super::DELETE_ROOM_ACTION)
		.ok_or_else(|| err!(Request(NotFound("Unknown delete task"))))?;

	Ok(ByDeleteIdResponse { status: delete_status(task)? })
}

/// # `GET /_synapse/admin/v2/rooms/{room_id}/delete_status`
///
/// Lists the room's deletion tasks. Tasks still scheduled are omitted, matching
/// Synapse, so a just-queued deletion is visible only by its delete id.
pub(crate) async fn admin_delete_status_by_room_route(
	State(services): State<crate::State>,
	body: Ruma<ByRoomIdRequest>,
) -> Result<ByRoomIdResponse> {
	require_admin(&services, body.sender_user()).await?;

	let results = services
		.tasks
		.by_resource(body.room_id.as_str())
		.into_iter()
		.filter(|task| {
			task.action == super::DELETE_ROOM_ACTION && task.status != Status::Scheduled
		})
		.map(delete_status)
		.collect::<Result<Vec<_>>>()?;

	if results.is_empty() {
		return Err!(Request(NotFound("No delete task for this room found")));
	}

	Ok(ByRoomIdResponse { results })
}

fn delete_status(task: TaskInfo) -> Result<DeleteStatus> {
	let room_id = RoomId::parse(&task.resource_id)
		.map_err(|_| err!(Request(NotFound("Delete task has an invalid room id"))))?;

	let shutdown_room = task
		.result
		.and_then(|result| serde_json::from_value(result).ok());

	Ok(DeleteStatus {
		delete_id: task.id.to_string(),
		room_id,
		status: delete_status_kind(task.status),
		shutdown_room,
	})
}

fn delete_status_kind(status: Status) -> DeleteStatusKind {
	match status {
		| Status::Scheduled => DeleteStatusKind::Scheduled,
		| Status::Active => DeleteStatusKind::Active,
		| Status::Complete => DeleteStatusKind::Complete,
		| Status::Failed => DeleteStatusKind::Failed,
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;
	use tuwunel_service::tasks::Status;

	use super::delete_status_kind;

	#[test]
	fn maps_each_stage_and_keeps_scheduled_distinct() {
		let kind = |status| serde_json::to_value(delete_status_kind(status)).unwrap();

		assert_eq!(kind(Status::Scheduled), json!("scheduled"));
		assert_eq!(kind(Status::Active), json!("active"));
		assert_eq!(kind(Status::Complete), json!("complete"));
		assert_eq!(kind(Status::Failed), json!("failed"));
	}
}
