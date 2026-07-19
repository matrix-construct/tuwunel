use axum::{
	extract::{Path, State},
	response::{IntoResponse, Response},
};
use http::StatusCode;
use tuwunel_core::{Result, err};

use super::ensure_enabled;

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn delete_route(
	State(services): State<crate::State>,
	Path(id): Path<String>,
) -> Result<Response> {
	ensure_enabled(&services)?;

	services
		.rendezvous
		.delete(&id)
		.then(|| StatusCode::NO_CONTENT.into_response())
		.ok_or_else(|| err!(Request(NotFound("Rendezvous session not found"))))
}
