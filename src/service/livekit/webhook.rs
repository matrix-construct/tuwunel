//! Receives LiveKit's webhooks (MSC4195).
//!
//! LiveKit signs each webhook with a token under the API key, and the token
//! carries a digest of the body. A webhook is never acted on alone: a
//! participant that joined is removed if its user has left the room since the
//! token was issued, and a participant reported joining, leaving or aborting
//! its connection has its delegation, if any, polled at once, which asks
//! LiveKit again.

use std::{
	borrow::Cow, collections::HashSet, fmt::Display, ops::Not, sync::LazyLock, time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures::future::join;
use http::HeaderValue;
use ruma::{OwnedRoomId, OwnedUserId};
use serde::Deserialize;
use tuwunel_core::{
	Error, Result,
	arrayvec::ArrayString,
	debug_warn, err, error, implement,
	jwt::{Algorithm, TokenData, Validation, decode},
	result::NotFound,
	utils::{BoolExt, hash::sha256::hash as sha256, time::now_millis},
};
use tuwunel_database::Deserialized;

use super::{ApiKey, Conf, Service, delegation::Participant};

// The padded standard base64 of a SHA-256 digest.
type Digest = ArrayString<DIGEST_LEN>;

/// The claims a webhook token carries beyond its lifetime.
///
/// The issuer is a single string, as LiveKit writes it. A join token, signed
/// with the same key and issuer, has no digest, so it never passes for a
/// webhook token.
#[derive(Deserialize)]
struct Claims {
	iss: ApiKey,
	sha256: Digest,
}

/// A webhook's body, as far as this server reads it.
///
/// LiveKit leaves out a field it has no value for, and every field this
/// server does not read is ignored.
#[derive(Deserialize)]
struct Notice<'a> {
	event: Event,

	#[serde(borrow)]
	room: Option<Room<'a>>,

	#[serde(borrow)]
	participant: Option<ParticipantInfo<'a>>,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Event {
	/// A participant connected, with media flowing.
	ParticipantJoined,

	/// A participant that had connected left.
	ParticipantLeft,

	/// A participant left before it ever connected, which LiveKit reports in
	/// place of a departure.
	ParticipantConnectionAborted,

	#[serde(other)]
	Other,
}

// LiveKit leaves out an empty name, as for the room of an egress that has none.
#[derive(Deserialize)]
struct Room<'a> {
	#[serde(borrow, default)]
	name: Cow<'a, str>,
}

#[derive(Deserialize)]
struct ParticipantInfo<'a> {
	#[serde(borrow, default)]
	identity: Cow<'a, str>,
}

const DIGEST_LEN: usize = 44;

/// Clock skew allowed on a webhook token's lifetime, as LiveKit allows.
const LEEWAY: Duration = Duration::from_mins(1);

/// What a webhook token must satisfy besides its claims' types.
///
/// It must be signed with HS256 under the API secret, carry an issuer and an
/// expiry, and fall within its lifetime give or take the leeway. The decoded
/// issuer must also equal the API key, which is checked in code, so this
/// needs no key of its own.
static VALIDATION: LazyLock<Validation> = LazyLock::new(|| Validation {
	required_spec_claims: HashSet::from(["exp".to_owned(), "iss".to_owned()]),
	leeway: LEEWAY.as_secs(),
	validate_nbf: true,
	..Validation::new(Algorithm::HS256)
});

/// Acts on a webhook from LiveKit.
///
/// The token in `Authorization` must verify under the API key and secret
/// before `body` is read, and the body must then have the digest the token
/// carries; any failure there is unauthorized, which LiveKit does not retry.
/// A participant joining has its user's membership checked if this server
/// recorded issuing it a token, and a participant joining, leaving or
/// aborting its connection has its delegation polled if it has one. Every
/// other event is ignored.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn webhook<Body>(&self, authorization: Option<&HeaderValue>, body: Body) -> Result
where
	Body: Future<Output = Result<Bytes>> + Send,
{
	let conf = self.conf()?;
	let Some((event, participant)) = conf.notice(authorization, body).await? else {
		return Ok(());
	};

	let now = now_millis();

	match event {
		| Event::Other => {},
		| Event::ParticipantLeft | Event::ParticipantConnectionAborted =>
			self.step(conf, &participant, now).await,
		| Event::ParticipantJoined => {
			let revalidated = self.revalidate(conf, &participant);
			let stepped = self.step(conf, &participant, now);

			join(revalidated, stepped).await;
		},
	}

	Ok(())
}

/// Verifies a webhook and reads the event it reports and the participant it
/// concerns.
///
/// `body` is read only once the token verifies, and its bytes are dropped
/// before the caller acts on the event. An event this server does not follow,
/// or one naming no participant this server could have issued a token for,
/// reads as none.
#[implement(Conf)]
#[tracing::instrument(level = "debug", skip_all)]
async fn notice<Body>(
	&self,
	authorization: Option<&HeaderValue>,
	body: Body,
) -> Result<Option<(Event, Participant)>>
where
	Body: Future<Output = Result<Bytes>> + Send,
{
	let digest = self.webhook_digest(authorization)?;
	let body = body.await?;

	signed(&digest, &body)?;

	let Notice { event, room, participant } = serde_json::from_slice(&body)
		.map_err(|error| err!(Request(BadJson("Unreadable LiveKit webhook: {error}"))))?;

	let participant = room
		.zip(participant)
		.filter(|_| event != Event::Other)
		.filter(|(room, info)| !room.name.is_empty() && !info.identity.is_empty())
		.and_then(|(room, info)| Participant::from_names(&room.name, &info.identity));

	Ok(participant.map(|participant| (event, participant)))
}

/// Verifies a webhook token, and returns the digest of the body it vouches
/// for.
///
/// The token proves only that LiveKit sent some body; the body read afterward
/// must still have this digest before anything in it is trusted.
#[implement(Conf)]
pub(super) fn webhook_digest(&self, authorization: Option<&HeaderValue>) -> Result<Digest> {
	let token = authorization
		.and_then(|value| value.to_str().ok())
		.ok_or_else(|| unsigned("no Authorization header"))?;

	let Claims { iss, sha256 } = decode(token, &self.webhook_key, &VALIDATION)
		.map(|decoded: TokenData<Claims>| decoded.claims)
		.map_err(unsigned)?;

	BoolExt::ok_or_else(iss == self.api_key, || unsigned("issuer is not the API key"))
		.map(|()| sha256)
}

/// Checks that a webhook's body has the digest its token carries.
///
/// `body` must be the bytes exactly as received: reserializing the JSON can
/// change them, and with them the digest.
pub(super) fn signed(digest: &Digest, body: &[u8]) -> Result {
	BoolExt::ok_or_else(*digest == body_digest(body), || unsigned("body digest differs"))
}

/// The digest of a webhook body, as LiveKit writes it into the token.
///
/// It is the padded standard base64 of the body's SHA-256.
pub(super) fn body_digest(body: &[u8]) -> Digest {
	let mut encoded = [0_u8; DIGEST_LEN];

	STANDARD
		.encode_slice(sha256(body), &mut encoded)
		.expect("32 bytes always encode to 44 padded base64 characters");

	Digest::from_byte_string(&encoded).expect("base64 output is always ASCII")
}

/// Removes a participant that joined LiveKit if its user is no longer joined
/// to the room it was issued a token for.
///
/// A participant this server has no record of is left alone, since another
/// issuer may share the API key, and so is one whose user's membership cannot
/// be read.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self, conf))]
async fn revalidate(&self, conf: &Conf, participant: &Participant) {
	let Some((room_id, user_id)) = self.issued_to(participant).await else {
		return;
	};

	let departed = self
		.services
		.state_cache
		.try_is_joined(&user_id, &room_id)
		.await
		.inspect_err(|error| error!(%error, "Failed to read the membership of a LiveKit user"))
		.is_ok_and(Not::not);

	if !departed {
		return;
	}

	let (room, identity) = participant.key();

	// Not found means it already left; any other failure was logged by the call.
	self.remove_participant(conf, room, identity)
		.await
		.ok();
}

/// Reads the room and user a participant was issued a token for.
///
/// A participant without a record reads as none, and so does an unreadable
/// record, which is logged.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn issued_to(&self, participant: &Participant) -> Option<(OwnedRoomId, OwnedUserId)> {
	self.db
		.lkparticipant_roomuserid
		.qry(&participant.key())
		.await
		.deserialized()
		.present()
		.inspect_err(
			|error| error!(?participant, %error, "Unreadable LiveKit participant record"),
		)
		.ok()
		.flatten()
}

// One answer for every failure, so a caller learns nothing of the token; the reason is logged.
fn unsigned<Reason: Display>(reason: Reason) -> Error {
	debug_warn!(%reason, "Refused a LiveKit webhook");
	err!(Request(Unauthorized("The LiveKit webhook signature is invalid.")))
}
