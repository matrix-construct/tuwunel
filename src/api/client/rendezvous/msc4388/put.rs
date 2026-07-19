use axum::extract::State;
use bytes::Bytes;
use ruma::api::client::rendezvous::update_rendezvous_session::unstable::{Request, Response};
use tuwunel_core::Err;
use tuwunel_service::rendezvous::Put;

use super::{Error, Result, ensure_available, ensure_data_size};
use crate::{ClientIp, Ruma};

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn put_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<Request>,
) -> Result<Response> {
	ensure_available(&services, client)?;
	ensure_data_size(&services, &body.data)?;

	let request = body.body;
	let data = Bytes::from(request.data);

	match services
		.rendezvous
		.put_token(&request.id, &request.sequence_token, data)
	{
		| Put::NotFound =>
			Err!(Request(NotFound("Rendezvous session not found"))).map_err(Into::into),
		| Put::PreconditionFailed(_) => Err(Error::ConcurrentWrite),
		| Put::Accepted(meta) => Ok(Response {
			sequence_token: meta.sequence_token().to_owned(),
		}),
	}
}
