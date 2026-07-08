use axum::extract::State;
use itertools::Itertools;
use ruma::{MilliSecondsSinceUnixEpoch, UInt};
use synapse_admin_api::scheduled_tasks::list::v1::{
	Request, Response, ScheduledTask, TaskStatus,
};
use tuwunel_core::Result;
use tuwunel_service::tasks::{Status, TaskInfo};

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/scheduled_tasks`
///
/// Lists the tracked background tasks, filtered by the query parameters and
/// ordered by increasing scheduled timestamp.
pub(crate) async fn admin_scheduled_tasks_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let scheduled_tasks = services
		.tasks
		.list()
		.into_iter()
		.filter(|task| matches_filters(task, &body))
		.sorted_by_key(|task| task.timestamp_ms)
		.map(scheduled_task)
		.collect();

	Ok(Response { scheduled_tasks })
}

fn matches_filters(task: &TaskInfo, req: &Request) -> bool {
	req.action_name
		.as_deref()
		.is_none_or(|name| task.action == name)
		&& req
			.resource_id
			.as_deref()
			.is_none_or(|id| task.resource_id.as_str() == id)
		&& req
			.status
			.as_ref()
			.is_none_or(|status| task_status(task.status) == *status)
		&& req
			.max_timestamp
			.is_none_or(|max| task.timestamp_ms <= u64::from(max))
}

fn scheduled_task(task: TaskInfo) -> ScheduledTask {
	ScheduledTask {
		id: task.id.to_string(),
		action: task.action.to_owned(),
		status: task_status(task.status),
		timestamp_ms: MilliSecondsSinceUnixEpoch(
			UInt::try_from(task.timestamp_ms).unwrap_or_default(),
		),
		resource_id: Some(task.resource_id),
		result: task
			.result
			.and_then(|value| serde_json::from_value(value).ok()),
		error: task.error,
	}
}

fn task_status(status: Status) -> TaskStatus {
	match status {
		| Status::Scheduled => TaskStatus::Scheduled,
		| Status::Active => TaskStatus::Active,
		| Status::Complete => TaskStatus::Complete,
		| Status::Failed => TaskStatus::Failed,
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;
	use tuwunel_service::tasks::Status;

	use super::task_status;

	#[test]
	fn maps_each_tracked_status() {
		let status = |status| serde_json::to_value(task_status(status)).unwrap();

		assert_eq!(status(Status::Scheduled), json!("scheduled"));
		assert_eq!(status(Status::Active), json!("active"));
		assert_eq!(status(Status::Complete), json!("complete"));
		assert_eq!(status(Status::Failed), json!("failed"));
	}
}
