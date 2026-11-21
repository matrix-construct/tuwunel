//! Calls LiveKit's RoomService through Twirp's JSON protocol.
//!
//! Each call posts a small JSON body with a short-lived API token on a client
//! that follows no redirect and is bounded by a timeout. Only an HTTP 404
//! carrying Twirp's `not_found` code (a participant LiveKit does not know)
//! passes through as not found, for callers to tell apart from failure. Every
//! other failure of CreateRoom or RemoveParticipant is logged here under the
//! method's name, never with LiveKit's message text, and becomes a 502 with no
//! detail, since LiveKit's own status (a 401 for a wrong key, say) would
//! mislead a Matrix client. A participant lookup, which a poller repeats every
//! few seconds, returns its failures unlogged to the poller that asked.

use std::{borrow::Cow, time::Duration};

use bytes::Bytes;
use http::StatusCode;
use serde::{
	Deserialize, Serialize,
	de::{DeserializeOwned, IgnoredAny},
};
use tuwunel_core::{
	Err, Error, Result, err, implement,
	utils::{BoolExt, time::now_secs},
	warn,
};
use url::{Position, Url};

use super::{Conf, Service, names::Name, token::Grant};
use crate::client::read_response_capped;

/// A RoomService method of the configured SFU.
///
/// The name is kept apart from the URL so that logs and spans can identify
/// the call without printing where it went.
pub(super) struct Method {
	/// The method's name, the only part of a request that is logged.
	pub(super) name: &'static str,

	pub(super) url: Url,
}

#[derive(Serialize)]
struct CreateRoom<'a> {
	name: &'a str,
}

#[derive(Debug, Deserialize)]
pub(super) struct Room {
	pub(super) name: Name,
}

#[derive(Serialize)]
struct ParticipantIdentity<'a> {
	room: &'a str,
	identity: &'a str,

	// Unix seconds; only RemoveParticipant reads it, taking a minute ahead when it is absent.
	#[serde(skip_serializing_if = "Option::is_none")]
	revoke_token_ts: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ParticipantInfo {
	#[serde(default)]
	state: State,
}

// LiveKit's participant states; an answer may leave out the first, protobuf's zero value.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum State {
	#[default]
	Joining,
	Joined,
	Active,
	Disconnected,
}

/// Whether a participant is connected, as following a delegation reads it.
///
/// LiveKit's `JOINING` and `JOINED` both read as joining, since only
/// `ACTIVE` means the participant's media flows. A participant LiveKit does
/// not know reads as gone, like a disconnected one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Presence {
	/// Connected, with media flowing.
	Active,

	/// Still setting up its connection.
	Joining,

	/// Disconnected, or unknown to LiveKit.
	Gone,
}

#[derive(Deserialize)]
struct TwirpError<'a> {
	#[serde(borrow)]
	code: Cow<'a, str>,
}

/// Bound on one RoomService call.
///
/// A token request waits on CreateRoom, so a stalled SFU must not hold the
/// client for long.
const API_TIMEOUT: Duration = Duration::from_secs(5);

// CreateRoom answers with an existing room whole, its metadata included, which
// LiveKit caps at 512 KiB by default; twice that bounds the answer.
const MAX_ANSWER: usize = 1024 * 1024;

impl Method {
	/// Locates a RoomService method from the SFU's WebSocket URL.
	///
	/// LiveKit serves its API on the same address over HTTP, so the scheme is
	/// swapped (`wss` to `https`, `ws` to `http`) and the path kept. Any
	/// credentials, query or fragment in the URL are dropped.
	pub(super) fn new(url: &str, name: &'static str) -> Result<Self> {
		let url = Url::parse(url)?;
		let scheme = match url.scheme() {
			| "wss" => "https",
			| "ws" => "http",
			| _ => return Err!(Config("livekit.url", "LiveKit url must be ws:// or wss://.")),
		};

		let address = url[Position::BeforeHost..Position::AfterPath].trim_end_matches('/');
		let url = Url::parse(&format!("{scheme}://{address}/twirp/livekit.RoomService/{name}"))?;

		Ok(Self { name, url })
	}
}

/// Creates a LiveKit room, or leaves an existing one as it is.
///
/// Only the name is sent, so the operator's LiveKit room settings apply. The
/// answer must name the requested room, so nothing but LiveKit's answer to
/// this request counts as success.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn create_room(&self, conf: &Conf, name: &str) -> Result {
	let request = CreateRoom { name };
	let method = &conf.create_room;
	let room = self
		.call(conf, method, Grant::create_room(name), &request)
		.await
		.map_err(|error| not_created(method.name, error))?;

	created(&room, name).map_err(|error| unavailable(method.name, &error))
}

/// Asks LiveKit whether a participant is connected to a room.
///
/// A participant LiveKit does not know, or reports disconnected, is gone. Any
/// failure returns to the caller unlogged; when LiveKit answered, the error
/// names its status and Twirp code, never its message text.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn get_participant(
	&self,
	conf: &Conf,
	room: &str,
	identity: &str,
) -> Result<Presence> {
	let request = ParticipantIdentity { room, identity, revoke_token_ts: None };
	let participant = self
		.exchange(conf, &conf.get_participant, Grant::room_admin(room), &request)
		.await
		.and_then(|(status, body)| answer(status, &body));

	presence(participant)
}

/// Asks LiveKit to remove a participant from a room, with the current time as
/// the cutoff for revoking its tokens.
///
/// A LiveKit that does not enforce revocation lets the participant reconnect
/// with a token it still holds. The caller gets a not-found error for a
/// participant LiveKit does not know; any other failure is logged and becomes
/// a 502.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn remove_participant(&self, conf: &Conf, room: &str, identity: &str) -> Result {
	let request = ParticipantIdentity {
		room,
		identity,
		revoke_token_ts: Some(now_secs()),
	};

	self.call(conf, &conf.remove_participant, Grant::room_admin(room), &request)
		.await
		.map(|_: IgnoredAny| ())
}

/// Posts one RoomService request and reads its answer.
///
/// An HTTP 404 with Twirp's `not_found` code passes through as not found for
/// the caller to branch on; every other failure is logged and becomes a 502.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		method = method.name,
	),
)]
async fn call<Request, Answer>(
	&self,
	conf: &Conf,
	method: &Method,
	grant: Grant<'_>,
	request: &Request,
) -> Result<Answer>
where
	Request: Serialize + Sync,
	Answer: DeserializeOwned,
{
	self.exchange(conf, method, grant, request)
		.await
		.and_then(|(status, body)| answer(status, &body))
		.map_err(|error| failed(method.name, error))
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn exchange<Request>(
	&self,
	conf: &Conf,
	method: &Method,
	grant: Grant<'_>,
	request: &Request,
) -> Result<(StatusCode, Bytes)>
where
	Request: Serialize + Sync,
{
	let token = conf.api_token(grant)?;
	let response = self
		.services
		.client
		.livekit
		.post(method.url.clone())
		.bearer_auth(token)
		.json(request)
		.timeout(API_TIMEOUT)
		.send()
		.await?;

	let status = response.status();
	let body = read_response_capped(response, MAX_ANSWER).await?;

	Ok((status, body))
}

/// Reads a RoomService answer.
///
/// A success parses as the method's answer, and an HTTP 404 with Twirp's
/// `not_found` code becomes not found. Anything else becomes an error naming
/// LiveKit's status and Twirp code, never its message text.
pub(super) fn answer<Answer>(status: StatusCode, body: &[u8]) -> Result<Answer>
where
	Answer: DeserializeOwned,
{
	if status.is_success() {
		return Ok(serde_json::from_slice(body)?);
	}

	match serde_json::from_slice(body).ok() {
		| None => Err!(BadServerResponse("LiveKit answered {status}")),
		| Some(TwirpError { code }) if status == StatusCode::NOT_FOUND && code == "not_found" =>
			Err!(Request(NotFound("LiveKit has no such participant."))),
		| Some(TwirpError { code }) => {
			let code = twirp_code(&code);

			Err!(BadServerResponse("LiveKit answered {status} {code}"))
		},
	}
}

// Twirp's codes are short lowercase words; anything else is not repeated.
fn twirp_code(code: &str) -> &str {
	let word = code.len() <= 32
		&& code
			.bytes()
			.all(|b| b.is_ascii_lowercase() || b == b'_');

	word.then_some(code).unwrap_or("unrecognized")
}

pub(super) fn failed(method: &str, error: Error) -> Error {
	if error.is_not_found() {
		return error;
	}

	unavailable(method, &error)
}

// CreateRoom cannot report a missing room, so a not-found answer is a bad gateway.
pub(super) fn not_created(method: &str, error: Error) -> Error {
	if error.is_not_found() {
		return unavailable(method, &error);
	}

	error
}

fn unavailable(method: &str, error: &Error) -> Error {
	warn!(%method, %error, "LiveKit RoomService call failed");
	err!(RequestStatus(BAD_GATEWAY, Unknown("The call server could not be reached.")))
}

pub(super) fn created(room: &Room, name: &str) -> Result {
	BoolExt::ok_or_else(room.name.as_str() == name, || {
		err!(BadServerResponse("LiveKit answered CreateRoom for another room."))
	})
}

pub(super) fn presence(participant: Result<ParticipantInfo>) -> Result<Presence> {
	match participant {
		| Err(error) if error.is_not_found() => Ok(Presence::Gone),
		| Err(error) => Err(error),
		| Ok(ParticipantInfo { state: State::Active }) => Ok(Presence::Active),
		| Ok(ParticipantInfo { state: State::Joining | State::Joined }) => Ok(Presence::Joining),
		| Ok(ParticipantInfo { state: State::Disconnected }) => Ok(Presence::Gone),
	}
}
