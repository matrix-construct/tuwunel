use axum::extract::State;
use ruma::api::client::rtc::transports::{
	msc4143::{Request as UnstableRequest, Response as UnstableResponse},
	v1::{Request, Response},
};
use tuwunel_core::Result;

use crate::Ruma;

/// Lists the MatrixRTC transports of MSC4143 on its stable path.
///
/// Answers `GET /_matrix/client/v1/rtc/transports` with the configured
/// transports in the server's order of preference. Only an authenticated
/// caller, guests included, receives them.
pub(crate) async fn get_transports_route(
	State(services): State<crate::State>,
	_body: Ruma<Request>,
) -> Result<Response> {
	Ok(Response {
		transports: services.config.rtc_transports()?,
	})
}

/// Lists the MatrixRTC transports of MSC4143 on its unstable path.
///
/// Answers `GET /_matrix/client/unstable/org.matrix.msc4143/rtc/transports`
/// with the same list as the stable path, under the `rtc_transports` key.
pub(crate) async fn get_transports_unstable_route(
	State(services): State<crate::State>,
	_body: Ruma<UnstableRequest>,
) -> Result<UnstableResponse> {
	Ok(UnstableResponse {
		rtc_transports: services.config.rtc_transports()?,
	})
}
