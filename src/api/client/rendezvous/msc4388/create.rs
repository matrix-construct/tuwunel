use axum::extract::State;
use bytes::Bytes;
use ruma::api::client::rendezvous::create_rendezvous_session::unstable_msc4388::{
	Request, Response,
};

use super::{Result, ensure_available, ensure_create_available, ensure_data_size};
use crate::{RateLimitIp, Ruma};

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn create_msc4388_route(
	State(services): State<crate::State>,
	RateLimitIp(client): RateLimitIp,
	body: Ruma<Request>,
) -> Result<Response> {
	ensure_available(&services, client)?;
	ensure_create_available(&services, &body)?;
	ensure_data_size(&services, &body.data)?;

	let (id, meta) = services
		.rendezvous
		.create_msc4388(Bytes::from(body.body.data));

	Ok(Response {
		id: id.as_str().to_owned(),
		sequence_token: meta.sequence_token().to_owned(),
		expires_in: meta.expires_in(),
	})
}
