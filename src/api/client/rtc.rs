use axum::extract::State;
use ruma::api::client::rtc::transports;
use tuwunel_core::Result;

use crate::Ruma;

/// # `GET /_matrix/client/unstable/org.matrix.msc4143/rtc/transports`
///
/// Get MatrixRTC transports for MSC4143.
pub(crate) async fn get_transports_route(
	State(services): State<crate::State>,
	_body: Ruma<transports::msc4143::Request>,
) -> Result<transports::msc4143::Response> {
	Ok(transports::msc4143::Response {
		rtc_transports: services.config.well_known.get_transports()?,
	})
}
