use axum::extract::State;
use futures::TryFutureExt;
use ruma::api::federation::rtc::livekit::get_token::{
	msc4195::{Request as UnstableRequest, Response as UnstableResponse},
	v1::{Request, Response},
};
use tuwunel_core::Result;

use crate::Ruma;

/// Issues a LiveKit token for another server's user (MSC4195).
///
/// Answers `POST /_matrix/federation/v1/rtc/livekit/get_token` when the call
/// runs on this server's SFU. The request must come from the user's server,
/// the user must be joined to the room, and this server must be in it.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_livekit_token_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	services
		.livekit
		.token_for_remote(body.origin(), &body.body)
		.map_ok(Response::new)
		.await
}

/// Issues a LiveKit token for another server's user on the unstable path.
///
/// Answers `POST /_matrix/federation/unstable/io.element.msc4195/rtc/livekit/get_token`
/// exactly as the stable path does.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_livekit_token_unstable_route(
	State(services): State<crate::State>,
	Ruma { body, origin, .. }: Ruma<UnstableRequest>,
) -> Result<UnstableResponse> {
	let origin = origin
		.as_deref()
		.expect("server must be authenticated for this handler");

	let UnstableRequest {
		user_id,
		url,
		room_id,
		slot_id,
		member_id,
		..
	} = body;

	let request = Request::new(user_id, url, room_id, slot_id, member_id);

	services
		.livekit
		.token_for_remote(origin, &request)
		.map_ok(UnstableResponse::new)
		.await
}
