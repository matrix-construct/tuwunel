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
		.and_then(|v| v.to_str().ok())
		.unwrap_or("");

	// Constant-time comparison to avoid timing side-channels.
	if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
		return (StatusCode::UNAUTHORIZED, "Invalid replication token").into_response();
	}

	next.run(request).await
}

/// Byte-by-byte constant-time equality check that does not short-circuit.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
	if a.len() != b.len() {
		// Still consume O(min(a,b)) time to avoid leaking which was shorter.
		a.iter()
			.zip(b.iter())
			.fold(0_u8, |acc, (x, y)| acc | (x ^ y));
		return false;
	}
	a.iter()
		.zip(b.iter())
		.fold(0_u8, |acc, (x, y)| acc | (x ^ y))
		== 0
}
