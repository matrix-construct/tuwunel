use axum::extract::State;
use http::HeaderMap;
use ruma::api::client::rendezvous::get_rendezvous_session::unstable::{Request, Response};
use tuwunel_core::Err;
use tuwunel_service::rendezvous::Get;

use super::{Result, data_to_string, ensure_available, ensure_safe_get};
use crate::{ClientIp, Ruma};

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	headers: HeaderMap,
	body: Ruma<Request>,
) -> Result<Response> {
	ensure_available(&services, client)?;
	ensure_safe_get(&headers)?;

	match services.rendezvous.get(&body.id, None) {
		| Get::NotFound =>
			Err!(Request(NotFound("Rendezvous session not found"))).map_err(Into::into),
		| Get::NotModified(_) =>
			Err!(Request(Unknown("Unconditional rendezvous read was not modified")))
				.map_err(Into::into),
		| Get::Data { data, meta } => Ok(Response {
			data: data_to_string(&data)?,
			sequence_token: meta.sequence_token().to_owned(),
			expires_in: meta.expires_in(),
		}),
	}
}
