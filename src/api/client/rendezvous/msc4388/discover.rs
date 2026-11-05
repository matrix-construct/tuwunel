use axum::extract::State;
use ruma::api::client::rendezvous::discover_rendezvous::unstable::{Request, Response};

use super::{Result, create_permitted, ensure_available};
use crate::{ClientIp, Ruma};

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn discover_msc4388_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<Request>,
) -> Result<Response> {
	ensure_available(&services, client)?;

	let create_available = create_permitted(&services, &body)?;

	Ok(Response { create_available })
}
