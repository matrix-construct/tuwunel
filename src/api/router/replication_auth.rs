use axum::{
	body::Body,
	extract::{Request, State},
	http::StatusCode,
	middleware::Next,
	response::{IntoResponse, Response},
};

pub(super) const TOKEN_HEADER: &str = "x-tuwunel-replication-token";

/// Axum middleware that validates the `X-Tuwunel-Replication-Token` header
/// against `config.rocksdb_replication_token`.
///
/// Returns:
/// - `501 Not Implemented` if replication is not configured on this instance.
/// - `401 Unauthorized` if the token is missing or incorrect.
/// - Passes through to the handler if the token matches.
pub(crate) async fn check_replication_token(
	State(services): State<super::State>,
	request: Request<Body>,
	next: Next,
) -> Response {
	let Some(ref expected) = services.server.config.rocksdb_replication_token else {
		return (StatusCode::NOT_IMPLEMENTED, "Replication is not configured on this instance")
			.into_response();
	};

	let provided = request
		.headers()
		.get(TOKEN_HEADER)
		.and_then(|v| v.to_str().ok());

	if provided != Some(expected) {
		return (StatusCode::UNAUTHORIZED, "Invalid replication token").into_response();
	}

	next.run(request).await
}
