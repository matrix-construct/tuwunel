use axum::extract::State;
use futures::TryFutureExt;
use ruma::api::client::rtc::livekit::delegate_delayed_leave::v1::{Request, Response};
use tuwunel_core::Result;

use crate::Ruma;

/// Delegates a call member's delayed leave event to this server (MSC4195).
///
/// Answers `POST /_matrix/client/v1/rtc/livekit/delegate_delayed_leave` and
/// its unstable path. The server then keeps the event from firing while the
/// member stays connected to its SFU, and sends it once the member leaves.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn delegate_delayed_leave_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	services
		.livekit
		.delegate(body.sender_user(), &body.body)
		.map_ok(|()| Response::new())
		.await
}
