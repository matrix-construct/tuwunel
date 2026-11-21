use axum::extract::{Request, State};
use futures::TryFutureExt;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HeaderName};
use tuwunel_core::Result;

use crate::router::read_body;

type Answer = ([(HeaderName, &'static str); 1], &'static str);

// Twirp encodes a successful empty response as `{}`.
const ANSWER: Answer = ([(CONTENT_TYPE, "application/json")], "{}");

/// Receives LiveKit's webhooks for native MatrixRTC calls (MSC4195).
///
/// Answers `POST /_tuwunel/livekit/webhook`, the URL an operator lists in
/// LiveKit's `webhook.urls`, with an empty object. LiveKit's signature admits
/// a request, not Matrix authentication, and the body is read only once its
/// token verifies.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn livekit_webhook_route(
	State(services): State<crate::State>,
	request: Request,
) -> Result<Answer> {
	let (parts, body) = request.into_parts();

	services
		.livekit
		.webhook(parts.headers.get(AUTHORIZATION), read_body(&services, body))
		.map_ok(|()| ANSWER)
		.await
}
