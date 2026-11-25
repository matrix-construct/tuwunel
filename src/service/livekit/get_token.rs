//! Answers `get_token` for local users and for other servers' users (MSC4195).
//!
//! Every refusal over the room or the user is the same 403, which does not
//! say which check failed; a `url` for another SFU is a 400, and a failure
//! upstream is a 502 with no detail.

use http::StatusCode;
use ruma::{
	ServerName, UserId,
	api::{
		client::rtc::livekit::get_token::v1::Request,
		error::{Error as RumaError, ErrorBody},
		federation::rtc::livekit::get_token::{
			msc4195::Request as UnstableRemoteRequest, v1::Request as RemoteRequest,
		},
	},
};
use serde_json::Value as JsonValue;
use tuwunel_core::{
	Error, Result, debug_warn, err, implement,
	utils::{BoolExt, FutureBoolExt, future::TryExtExt},
};

use super::{Service, names::unstable_slot};

/// Answers a local user's `get_token` request.
///
/// The user must be joined to the room. A call on this server's SFU gets a
/// token signed here, for the configured `url` alone; a call on another
/// server's SFU gets one from that server, which must be in the room too.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%sender_user,
	),
)]
pub async fn token_for(&self, sender_user: &UserId, request: &Request) -> Result<String> {
	let joined = self
		.services
		.state_cache
		.is_joined(sender_user, &request.room_id)
		.await;

	BoolExt::ok_or_else(joined, forbidden)?;

	match request.server_name.as_deref() {
		| Some(server_name) if !self.services.globals.server_is_ours(server_name) =>
			self.remote_token(server_name, sender_user, request)
				.await,
		| _ => {
			BoolExt::ok_or_else(self.is_own_url(&request.url), foreign_url)?;

			self.issue_token(sender_user, &request.room_id, &request.slot_id, &request.member_id)
				.await
		},
	}
}

/// Answers another server's `get_token` request for one of its users.
///
/// The user must belong to the origin and be joined, this server must be in
/// the room, and the origin must pass the room's ACL. A refusal by any of
/// these reads alike, so the origin learns nothing of which one failed.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%origin,
	),
)]
pub async fn token_for_remote(
	&self,
	origin: &ServerName,
	request: &RemoteRequest,
) -> Result<String> {
	let (user_id, room_id) = (&request.user_id, &request.room_id);
	let state_cache = &self.services.state_cache;
	let joined = state_cache
		.server_in_room(self.services.globals.server_name(), room_id)
		.and(state_cache.is_joined(user_id, room_id));

	// The ACL check reads and parses a state event, so it runs only after both joins.
	let permitted = user_id.server_name() == origin
		&& joined.await
		&& self
			.services
			.event_handler
			.acl_check(origin, room_id)
			.is_ok()
			.await;

	BoolExt::ok_or_else(permitted, forbidden)?;
	BoolExt::ok_or_else(self.is_own_url(&request.url), foreign_url)?;

	self.issue_token(user_id, room_id, &request.slot_id, &request.member_id)
		.await
}

/// Fetches a token from the server whose SFU hosts the call.
///
/// A server without the stable endpoint gets one retry on the unstable path,
/// as lk-jwt-service does, and only that server's refusals reach the client.
#[implement(Service)]
async fn remote_token(
	&self,
	server_name: &ServerName,
	sender_user: &UserId,
	request: &Request,
) -> Result<String> {
	let in_room = self
		.services
		.state_cache
		.server_in_room(server_name, &request.room_id)
		.await;

	BoolExt::ok_or_else(in_room, forbidden)?;

	let federation = &self.services.federation;
	let client = &self.services.client.livekit_federation;
	let stable = stable_request(sender_user, request);
	let jwt = match federation
		.execute_on(client, server_name, stable, None)
		.await
	{
		| Err(error) if is_unrecognized(&error) => {
			let unstable = unstable_request(sender_user, request);

			federation
				.execute_on(client, server_name, unstable, None)
				.await
				.map(|response| response.jwt)
		},
		| response => response.map(|response| response.jwt),
	};

	jwt.map_err(|error| relay_error(&error))
}

/// Builds the federation request for the room's server.
///
/// It carries the slot as the client sent it, for a server with the stable
/// endpoint, which folds the call slot itself.
pub(super) fn stable_request(user_id: &UserId, request: &Request) -> RemoteRequest {
	RemoteRequest::new(
		user_id.to_owned(),
		request.url.clone(),
		request.room_id.clone(),
		request.slot_id.clone(),
		request.member_id.clone(),
	)
}

// lk-jwt-service retries on a 404 carrying `M_UNRECOGNIZED` or no errcode.
pub(super) fn is_unrecognized(error: &Error) -> bool {
	let Error::Federation(_, remote) = error else {
		return false;
	};

	answered(remote, StatusCode::NOT_FOUND, &["", "M_UNRECOGNIZED"])
}

/// Builds the retry for a server without the stable endpoint.
///
/// Such a server predates the call-slot fold and hashes slots as sent, so the
/// retry carries the call slot in the spelling its clients send.
pub(super) fn unstable_request(user_id: &UserId, request: &Request) -> UnstableRemoteRequest {
	UnstableRemoteRequest::new(
		user_id.to_owned(),
		request.url.clone(),
		request.room_id.clone(),
		unstable_slot(&request.slot_id).to_owned(),
		request.member_id.clone(),
	)
}

/// Maps another server's answer to what the client may see.
///
/// Only a 403 `M_FORBIDDEN` and a 400 `M_INVALID_PARAM` pass on, as the same
/// generic refusals this server gives; anything else, a transport failure
/// included, is a 502 with no detail.
pub(super) fn relay_error(error: &Error) -> Error {
	match error {
		| Error::Federation(_, remote)
			if answered(remote, StatusCode::FORBIDDEN, &["M_FORBIDDEN"]) =>
			forbidden(),
		| Error::Federation(_, remote)
			if answered(remote, StatusCode::BAD_REQUEST, &["M_INVALID_PARAM"]) =>
			foreign_url(),
		| _ => {
			debug_warn!(%error, "The room's server issued no LiveKit token");
			err!(RequestStatus(BAD_GATEWAY, Unknown("The room's call server issued no token.")))
		},
	}
}

/// Whether another server answered with this status and one of these
/// errcodes.
///
/// The errcode is read from the body as lk-jwt-service reads it, whether or
/// not the body is a well-formed Matrix error, and an absent one is empty.
fn answered(remote: &RumaError, status: StatusCode, errcodes: &[&str]) -> bool {
	let accepted = |errcode: &str| errcodes.contains(&errcode);

	remote.status_code == status
		&& match &remote.body {
			| ErrorBody::Standard(body) => accepted(body.kind.errcode().as_str()),
			| ErrorBody::Json(json) => accepted(
				json.get("errcode")
					.and_then(JsonValue::as_str)
					.unwrap_or_default(),
			),
			| ErrorBody::NotJson { .. } => accepted(""),
		}
}

// lk-jwt-service's wording, for every refusal alike.
pub(super) fn forbidden() -> Error {
	err!(Request(Forbidden("Access to this room is not permitted.")))
}

fn foreign_url() -> Error { err!(Request(InvalidParam("No token is issued for that url."))) }
