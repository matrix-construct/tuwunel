use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use http::StatusCode;
use http_body_util::Full;
use ruma::api::{OutgoingResponse, client::uiaa::UiaaResponse};
use tuwunel_core::{Error, error};

pub(crate) struct RumaResponse<T>(pub(crate) T)
where
	T: OutgoingResponse;

impl From<Error> for RumaResponse<UiaaResponse> {
	fn from(t: Error) -> Self {
		Self(t.into())
	}
}

impl<T> IntoResponse for RumaResponse<T>
where
	T: OutgoingResponse,
{
	fn into_response(self) -> Response {
		self.0
			.try_into_http_response::<BytesMut>()
			.inspect_err(|e| error!("response error: {e}"))
			.map_or_else(
				|_| StatusCode::INTERNAL_SERVER_ERROR.into_response(),
				|r| {
					r.map(BytesMut::freeze)
						.map(Full::new)
						.into_response()
				},
			)
	}
}
