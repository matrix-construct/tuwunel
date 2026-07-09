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

/// # `GET /_tuwunel/remote_version/{server_name}`
///
/// Tuwunel-specific API to probe a remote server's
/// `/_matrix/federation/v1/version` endpoint, returning that response body
/// along with the round-trip time of the probe.
pub(crate) async fn tuwunel_remote_version(
	State(services): State<crate::State>,
	body: Ruma<get_remote_version::unstable::Request>,
) -> Result<get_remote_version::unstable::Response> {
	let timer = Instant::now();

	let server = if services.globals.server_is_ours(&body.server_name) {
		Some(get_server_version::v1::Server {
			name: Some(tuwunel_core::version::name().into()),
			version: Some(tuwunel_core::version::version().into()),
			compiler: tuwunel_core::info::rustc::version().map(Into::into),
			..Default::default()
		})
	} else {
		services
			.federation
			.execute(&body.server_name, get_server_version::v1::Request {})
			.await?
			.server
	};

	let elapsed = timer.elapsed();

	Ok(get_remote_version::unstable::Response {
		data: serde_json::value::to_raw_value(&server)?,
		rtt_ms: elapsed,
	})
}
