use std::time::Instant;

use axum::{Json, extract::State, response::IntoResponse};
use futures::StreamExt;
use ruma::api::{client::tuwunel::get_remote_version, federation::discovery::get_server_version};
use tuwunel_core::Result;

use crate::Ruma;

/// # `GET /_tuwunel/server_version`
///
/// Tuwunel-specific API to get the server version, results akin to
/// `/_matrix/federation/v1/version`
pub(crate) async fn tuwunel_server_version() -> Result<impl IntoResponse> {
	Ok(Json(serde_json::json!({
		"name": tuwunel_core::version::name(),
		"version": tuwunel_core::version::version(),
	})))
}

/// # `GET /_tuwunel/local_user_count`
///
/// Tuwunel-specific API to return the amount of users registered on this
/// homeserver. Endpoint is disabled if federation is disabled for privacy. This
/// only includes active users (not deactivated, no guests, etc)
pub(crate) async fn tuwunel_local_user_count(
	State(services): State<crate::State>,
) -> Result<impl IntoResponse> {
	let user_count = services.users.list_local_users().count().await;

	Ok(Json(serde_json::json!({
		"count": user_count
	})))
}

pub(crate) async fn tuwunel_remote_version(
	State(services): State<crate::State>,
	body: Ruma<get_remote_version::unstable::Request>,
) -> Result<get_remote_version::unstable::Response> {
	let timer = Instant::now();

	let response = services
		.federation
		.execute(&body.server_name, get_server_version::v1::Request {})
		.await?;

	let elapsed = timer.elapsed();

	Ok(get_remote_version::unstable::Response {
		data: serde_json::value::to_raw_value(&response.server)?,
		rtt_ms: elapsed,
	})
}
