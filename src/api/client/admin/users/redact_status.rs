use std::collections::BTreeMap;

use axum::extract::State;
use ruma::OwnedEventId;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use synapse_admin_api::users::redact_status::v1::{RedactStatus, Request, Response};
use tuwunel_core::{Result, err};
use tuwunel_service::tasks::{Status, TaskInfo};

use crate::{Ruma, client::admin::require_admin};

/// The payload the redaction task records; an unexpected shape folds to empty.
#[derive(Default, Deserialize)]
struct RedactOutcome {
	#[serde(default)]
	failed_redactions: BTreeMap<OwnedEventId, String>,
}

/// # `GET /_synapse/admin/v1/user/redact_status/{redact_id}`
///
/// Reports the stage of a bulk user-event redaction by its task id, including
/// the per-event failures once the task has completed.
pub(crate) async fn admin_redact_status_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let task = services
		.tasks
		.get(&body.redact_id)
		.filter(|task| task.action == super::REDACT_USER_ACTION)
		.ok_or_else(|| err!(Request(NotFound("Unknown redact task"))))?;

	Ok(redact_response(task))
}

fn redact_response(task: TaskInfo) -> Response {
	let failed_redactions = (task.status == Status::Complete).then(|| {
		task.result
			.map(completed_failures)
			.unwrap_or_default()
	});

	Response {
		status: redact_status(task.status),
		failed_redactions,
		error: task.error,
	}
}

fn completed_failures(result: JsonValue) -> BTreeMap<OwnedEventId, String> {
	let outcome: RedactOutcome = serde_json::from_value(result).unwrap_or_default();

	outcome.failed_redactions
}

fn redact_status(status: Status) -> RedactStatus {
	match status {
		| Status::Scheduled => RedactStatus::Scheduled,
		| Status::Active => RedactStatus::Active,
		| Status::Complete => RedactStatus::Complete,
		| Status::Failed => RedactStatus::Failed,
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;
	use tuwunel_service::tasks::Status;

	use super::{completed_failures, redact_status};

	#[test]
	fn maps_each_stage_and_emits_complete() {
		let status = |status| serde_json::to_value(redact_status(status)).unwrap();

		assert_eq!(status(Status::Scheduled), json!("scheduled"));
		assert_eq!(status(Status::Active), json!("active"));
		assert_eq!(status(Status::Complete), json!("complete"));
		assert_eq!(status(Status::Failed), json!("failed"));
	}

	#[test]
	fn folds_unexpected_result_shapes_to_no_failures() {
		assert!(completed_failures(json!(null)).is_empty());
		assert!(completed_failures(json!({})).is_empty());
		assert!(completed_failures(json!({ "purged": 7 })).is_empty());

		let failures =
			completed_failures(json!({ "failed_redactions": { "$f:example.com": "boom" } }));

		assert_eq!(failures.len(), 1);
	}
}
