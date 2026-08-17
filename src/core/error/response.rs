use bytes::BytesMut;
use http::StatusCode;
use http_body_util::Full;
use ruma::{
	ServerName,
	api::{
		OutgoingResponse,
		client::uiaa::UiaaResponse,
		error::{Error as RumaError, ErrorBody, ErrorKind, StandardErrorBody},
	},
};

use super::Error;
use crate::error;

impl axum::response::IntoResponse for Error {
	fn into_response(self) -> axum::response::Response {
		let response: UiaaResponse = self.into();
		response
			.try_into_http_response::<BytesMut>()
			.inspect_err(|e| error!("error response error: {e}"))
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

impl From<Error> for UiaaResponse {
	#[inline]
	fn from(error: Error) -> Self {
		if let Error::Uiaa(uiaainfo) = error {
			return Self::AuthResponse(uiaainfo);
		}

		let status = match &error {
			| Error::Federation(origin, remote) if !is_relayable(ruma_error_kind(remote)) =>
				return withheld_remote_error(origin),

			// A remote's 401 reads to a client as its own session failing.
			| Error::Federation(..) if error.status_code() == StatusCode::UNAUTHORIZED =>
				StatusCode::BAD_REQUEST,

			| _ => error.status_code(),
		};

		matrix_response(status, error.kind(), error.message())
	}
}

/// Whether a remote server's error may be repeated to a local client.
///
/// A remote answers only for the resource it was asked about, so a kind
/// describing the caller's own session or this server's state is withheld: the
/// client has no way to tell the two apart and would act on it as ours.
fn is_relayable(kind: &ErrorKind) -> bool {
	use ErrorKind::*;

	matches!(
		kind,
		Forbidden
			| NotFound
			| UnsupportedRoomVersion
			| IncompatibleRoomVersion(..)
			| InviteBlocked
			| UnableToAuthorizeJoin
			| UnableToGrantJoin
	)
}

fn withheld_remote_error(origin: &ServerName) -> UiaaResponse {
	let message = format!("Request to {origin} failed.");

	matrix_response(StatusCode::BAD_GATEWAY, ErrorKind::Unknown, message)
}

fn matrix_response(status: StatusCode, kind: ErrorKind, message: String) -> UiaaResponse {
	let body = ErrorBody::Standard(StandardErrorBody { kind, message });

	UiaaResponse::MatrixError(RumaError::new(status, body))
}

pub(super) fn status_code(kind: &ErrorKind, hint: StatusCode) -> StatusCode {
	if hint == StatusCode::BAD_REQUEST {
		bad_request_code(kind)
	} else {
		hint
	}
}

pub(super) fn bad_request_code(kind: &ErrorKind) -> StatusCode {
	use ErrorKind::*;

	match kind {
		// 504
		| NotYetUploaded | ConnectionTimeout => StatusCode::GATEWAY_TIMEOUT,

		// 502
		| BadStatus(..) | ConnectionFailed => StatusCode::BAD_GATEWAY,

		// 429
		| LimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,

		// 413
		| TooLarge => StatusCode::PAYLOAD_TOO_LARGE,

		// 409
		| CannotOverwriteMedia => StatusCode::CONFLICT,

		// 404
		| NotFound | NotImplemented | FeatureDisabled | Unrecognized => StatusCode::NOT_FOUND,

		// 403
		| GuestAccessForbidden
		| ThreepidAuthFailed
		| UserDeactivated
		| ThreepidDenied
		| InviteBlocked
		| WrongRoomKeysVersion { .. }
		| Forbidden => StatusCode::FORBIDDEN,

		// 401
		| UnknownToken { .. } | MissingToken | Unauthorized | UserLocked =>
			StatusCode::UNAUTHORIZED,

		// 400
		| _ => StatusCode::BAD_REQUEST,
	}
}

pub(super) fn ruma_error_message(error: &RumaError) -> String {
	if let ErrorBody::Standard(StandardErrorBody { message, .. }) = &error.body {
		return message.clone();
	}

	format!("{error}")
}

pub(super) fn ruma_error_kind(e: &RumaError) -> &ErrorKind {
	e.error_kind().unwrap_or(&ErrorKind::Unknown)
}

pub(super) fn io_error_code(kind: std::io::ErrorKind) -> StatusCode {
	use std::io::ErrorKind;

	match kind {
		| ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
		| ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
		| ErrorKind::NotFound => StatusCode::NOT_FOUND,
		| ErrorKind::TimedOut => StatusCode::GATEWAY_TIMEOUT,
		| ErrorKind::FileTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
		| ErrorKind::StorageFull => StatusCode::INSUFFICIENT_STORAGE,
		| ErrorKind::Interrupted => StatusCode::SERVICE_UNAVAILABLE,
		| _ => StatusCode::INTERNAL_SERVER_ERROR,
	}
}
