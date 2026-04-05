use axum::{Json, extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tuwunel_core::Err;
use url::Url;

/// Request body for `POST /_tuwunel/cluster/demote`.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DemoteBody {
	/// URL of the new primary to replicate from (e.g. `http://host:8008`).
	primary_url: Url,
}

/// `POST /_tuwunel/cluster/demote`
///
/// Demotes this promoted primary back to a secondary that replicates from
/// `primary_url`. Resets the resume cursor and triggers a fresh checkpoint
/// bootstrap from the new primary — the worker restarts replication without
/// requiring a process restart.
///
/// Typical use case: the original primary comes back online after a failover
/// and needs to re-join the cluster as a secondary under the newly promoted
/// node.
///
/// Returns:
/// - `200 OK` with `{"status":"demoted","primary_url":"..."}` on success.
/// - `400 Bad Request` if `primary_url` is missing or empty.
/// - `409 Conflict` if this instance is not currently promoted (i.e. it is
///   already actively replicating or was never a secondary).
#[tracing::instrument(level = "info", skip_all, fields(?body))]
pub(crate) async fn demote_route(
	State(services): State<crate::State>,
	Json(body): Json<DemoteBody>,
) -> impl IntoResponse {
	if body.primary_url.as_str().is_empty() {
		return Err!(HttpJson(BAD_REQUEST, {"error": "primary_url is required"}));
	}

	if let Err(e) = services
		.cluster
		.demote(body.primary_url.clone())
		.await
	{
		return Err!(HttpJson(CONFLICT, {"error": e.to_string()}));
	}

	Ok(Json(json!({
		"status": "demoted",
		"primary_url": body.primary_url,
	})))
}
