use axum::{Json, extract::State, response::IntoResponse};
use serde_json::json;
use tuwunel_core::Err;

/// `POST /_tuwunel/cluster/promote`
///
/// Promotes this secondary to a standalone primary by stopping the replication
/// worker. After this call returns the instance accepts writes and no longer
/// tails the primary's WAL. The caller is responsible for updating the VIP or
/// load balancer to route client traffic to this node.
///
/// Returns:
/// - `200 OK` with `{"status":"promoted"}` on success.
/// - `409 Conflict` if this instance is already a primary (no
///   `rocksdb_primary_url` was configured, or it was already promoted).
#[tracing::instrument(name = "promote", level = "info", skip_all)]
pub(crate) async fn promote_route(State(services): State<crate::State>) -> impl IntoResponse {
	if services.cluster.is_promoted() {
		return Err!(HttpJson(CONFLICT, {"error": "already promoted"}));
	}

	if services
		.server
		.config
		.rocksdb_primary_url
		.is_none()
	{
		return Err!(HttpJson(CONFLICT, {
			"error": "not a secondary; no rocksdb_primary_url configured"
		}));
	}

	services.cluster.promote();

	Ok(Json(json!({"status": "promoted"})))
}
