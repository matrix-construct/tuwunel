use axum::extract::State;
use futures::TryFutureExt;
use ruma::api::client::rtc::livekit::get_token::v1::{Request, Response};
use tuwunel_core::Result;

use crate::Ruma;

/// Issues a LiveKit token for a MatrixRTC call (MSC4195).
///
/// Answers `POST /_matrix/client/v1/rtc/livekit/get_token` and its unstable
/// path. A call on this server's SFU gets a token signed here, and a call on
/// another server's SFU one fetched from that server.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_livekit_token_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	services
		.livekit
		.token_for(body.sender_user(), &body.body)
		.map_ok(Response::new)
		.await
}
