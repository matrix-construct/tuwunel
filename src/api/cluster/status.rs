use axum::{Json, extract::State, response::IntoResponse};
use serde_json::json;

/// `GET /_tuwunel/cluster/status`
///
/// Returns the primary's current WAL sequence number and role.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_status(State(services): State<crate::State>) -> impl IntoResponse {
	let db = services.db.clone();
	let seq = services
		.server
		.runtime()
		.spawn_blocking(move || db.engine.current_sequence())
		.await
		.unwrap_or(0);

	let role = services
		.server
		.config
		.rocksdb_primary_url
		.as_ref()
		.filter(|_| !services.cluster.is_promoted())
		.and(Some("secondary"))
		.unwrap_or("primary");

	Json(json!({
		"role": role,
		"latest_sequence": seq,
	}))
}
