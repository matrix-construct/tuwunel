use std::result::Result as StdResult;

use axum::{
	body::Body,
	extract::{Path, State, rejection::PathRejection},
};
use bytes::Bytes;
use ruma::{
	OwnedTransactionId, TransactionId,
	api::client::rendezvous::update_rendezvous_session::unstable::Response,
};
use serde::Deserialize;
use serde_json::{Value as JsonValue, from_slice, from_value};
use tuwunel_core::{Err, Error as CoreError, Result as CoreResult, err, smallstr::SmallString};
use tuwunel_service::{
	Services,
	rendezvous::{Attempt, Meta, Put},
};

use super::{Error, Result, ensure_available, ensure_data_size, ensure_txn_id};
use crate::{ClientIp, RumaResponse, router::read_body};

type Id = SmallString<[u8; 32]>;
type SequenceToken = SmallString<[u8; 43]>;
type PathParams = (Id, OwnedTransactionId);

#[derive(Deserialize)]
struct SendBody {
	sequence_token: SequenceToken,
	data: String,
}

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn put_msc4388_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	path: StdResult<Path<PathParams>, PathRejection>,
	body: Body,
) -> Result<RumaResponse<Response>> {
	ensure_available(&services, client)?;

	let Path((id, txn_id)) = path.map_err(CoreError::from)?;

	ensure_txn_id(&txn_id)?;

	// MSC4388: a recorded transaction is answered before the body is validated,
	// which the Ruma extractor would parse first.
	let meta = match services.rendezvous.recorded(&id, &txn_id) {
		| Some(meta) => meta,
		| None => send(&services, &id, &txn_id, body).await?,
	};

	let sequence_token = meta.sequence_token().to_owned();

	Ok(RumaResponse(Response { sequence_token }))
}

async fn send(services: &Services, id: &str, txn_id: &TransactionId, body: Body) -> Result<Meta> {
	let SendBody { sequence_token, data } = parse_body(services, body).await?;

	ensure_data_size(services, &data)?;

	let attempt = Attempt { txn_id, sequence_token: &sequence_token };

	match services
		.rendezvous
		.put_token(id, attempt, Bytes::from(data))
	{
		| Put::NotFound =>
			Err!(Request(NotFound("Rendezvous session not found"))).map_err(Into::into),
		| Put::PreconditionFailed(_) => Err(Error::ConcurrentWrite),
		| Put::Accepted(meta) => Ok(meta),
	}
}

async fn parse_body(services: &Services, body: Body) -> CoreResult<SendBody> {
	let body = read_body(services, body).await?;

	// As the Ruma extractor: syntax outranks a field error, and a duplicate key
	// keeps its last value.
	let json: JsonValue = from_slice(&body)
		.map_err(|_| err!(Request(NotJson("Request body is not valid JSON."))))?;

	from_value(json).map_err(|e| err!(Request(BadJson("Request body is malformed: {e}"))))
}
