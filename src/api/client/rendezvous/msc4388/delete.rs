use axum::extract::State;
use ruma::api::client::rendezvous::delete_rendezvous_session::unstable::{Request, Response};
use tuwunel_core::err;

use super::{Result, ensure_available};
use crate::{ClientIp, Ruma};

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn delete_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<Request>,
) -> Result<Response> {
	ensure_available(&services, client)?;

	services
		.rendezvous
		.delete_if_active(&body.id)
		.then(Response::new)
		.ok_or_else(|| err!(Request(NotFound("Rendezvous session not found"))))
		.map_err(Into::into)
}
