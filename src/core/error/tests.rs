use std::borrow::Cow;

use http::StatusCode;
use ruma::{
	OwnedServerName, RoomVersionId,
	api::{
		client::uiaa::UiaaResponse,
		error::{
			Error as RumaError, ErrorBody, ErrorKind, IncompatibleRoomVersionErrorData,
			StandardErrorBody, UnknownTokenErrorData, UserLimitExceededErrorData,
		},
	},
};

use super::{Error, response::ruma_error_kind};

const REMOTE_MESSAGE: &str = "your session was revoked";

#[test]
fn a_remote_unknown_token_is_withheld() {
	let (status, kind) = client_response(remote(unknown_token(), StatusCode::UNAUTHORIZED));

	assert_eq!(status, StatusCode::BAD_GATEWAY, "the remote status is dropped");
	assert_eq!(kind, ErrorKind::Unknown, "the remote errcode is dropped");
}

#[test]
fn our_own_unknown_token_still_reaches_the_client() {
	let ours = [
		Error::BadRequest(unknown_token(), "Unknown access token."),
		Error::Request(unknown_token(), Cow::Borrowed(""), StatusCode::UNAUTHORIZED),
	];

	for error in ours {
		let (status, kind) = client_response(error);

		assert_eq!(status, StatusCode::UNAUTHORIZED, "our own token errors are unaffected");
		assert_eq!(kind, unknown_token());
	}
}

#[test]
fn a_locked_account_error_carries_a_401() {
	let ours = [
		Error::BadRequest(ErrorKind::UserLocked, "This account has been locked."),
		Error::Request(ErrorKind::UserLocked, Cow::Borrowed(""), StatusCode::BAD_REQUEST),
	];

	for error in ours {
		let (status, kind) = client_response(error);

		assert_eq!(status, StatusCode::UNAUTHORIZED, "MSC3939 requires 401 for a locked account");
		assert_eq!(kind, ErrorKind::UserLocked);
	}
}

#[test]
fn a_remote_uri_bearing_error_is_withheld() {
	let user_limit = ErrorKind::UserLimitExceeded(UserLimitExceededErrorData {
		info_uri: "https://remote.example/pay".to_owned(),
		can_upgrade: false,
	});

	let (_, kind) = client_response(remote(user_limit, StatusCode::FORBIDDEN));

	assert_eq!(kind, ErrorKind::Unknown, "a remote-chosen uri does not reach the client");
}

#[test]
fn a_remote_room_error_is_relayed() {
	for (remote_kind, remote_status) in [
		(ErrorKind::Forbidden, StatusCode::FORBIDDEN),
		(ErrorKind::NotFound, StatusCode::NOT_FOUND),
		(ErrorKind::UnsupportedRoomVersion, StatusCode::BAD_REQUEST),
		(
			ErrorKind::IncompatibleRoomVersion(IncompatibleRoomVersionErrorData::new(
				RoomVersionId::V11,
			)),
			StatusCode::BAD_REQUEST,
		),
		(ErrorKind::InviteBlocked, StatusCode::FORBIDDEN),
		(ErrorKind::UnableToAuthorizeJoin, StatusCode::BAD_REQUEST),
		(ErrorKind::UnableToGrantJoin, StatusCode::BAD_REQUEST),
	] {
		let (status, kind) = client_response(remote(remote_kind.clone(), remote_status));

		assert_eq!(status, remote_status, "the room's own failure keeps its status");
		assert_eq!(kind, remote_kind, "the room's own failure keeps its errcode");
	}
}

#[test]
fn a_withheld_error_does_not_quote_the_remote() {
	let message = message_of(remote(unknown_token(), StatusCode::UNAUTHORIZED));

	assert!(!message.contains(REMOTE_MESSAGE), "the remote's own text is not repeated");
	assert!(message.contains("remote.example"), "the origin is still named");
}

#[test]
fn a_relayed_error_never_keeps_a_401() {
	let (status, kind) = client_response(remote(ErrorKind::Forbidden, StatusCode::UNAUTHORIZED));

	assert_eq!(status, StatusCode::BAD_REQUEST, "a remote cannot answer 401 through us");
	assert_eq!(kind, ErrorKind::Forbidden, "its errcode is still relayed");
}

#[test]
fn a_relayed_error_attributes_the_remote() {
	let message = message_of(remote(ErrorKind::Forbidden, StatusCode::FORBIDDEN));

	assert!(
		message.contains("remote.example"),
		"a relayed message carries the remote's own text, so it must name whose it is"
	);
}

fn unknown_token() -> ErrorKind {
	ErrorKind::UnknownToken(UnknownTokenErrorData { soft_logout: false })
}

fn remote(kind: ErrorKind, status: StatusCode) -> Error {
	let origin = OwnedServerName::try_from("remote.example").expect("the server name parses");
	let body =
		ErrorBody::Standard(StandardErrorBody { kind, message: REMOTE_MESSAGE.to_owned() });

	Error::Federation(origin, body.into_error(status))
}

fn client_response(error: Error) -> (StatusCode, ErrorKind) {
	let error = matrix_error(error);
	let kind = ruma_error_kind(&error).clone();

	(error.status_code, kind)
}

fn message_of(error: Error) -> String {
	let ErrorBody::Standard(StandardErrorBody { message, .. }) = matrix_error(error).body else {
		panic!("the response carries a standard body")
	};

	message
}

fn matrix_error(error: Error) -> RumaError {
	let UiaaResponse::MatrixError(error) = UiaaResponse::from(error) else {
		panic!("a non-uiaa error becomes a matrix error")
	};

	error
}
