use axum::extract::State;
use ruma::api::client::rendezvous::discover_rendezvous::unstable::{Request, Response};

use super::{Result, ensure_available, ensure_create_available};
use crate::{ClientIp, Ruma};

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn discover_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<Request>,
) -> Result<Response> {
	ensure_available(&services, client)?;
	ensure_create_available(&services, &body)?;

	Ok(Response { create_available: true })
}
