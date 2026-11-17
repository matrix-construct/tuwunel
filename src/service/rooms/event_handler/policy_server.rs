use std::{
	collections::BTreeMap,
	time::{Duration, UNIX_EPOCH},
};

use http::StatusCode;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedServerName, RoomId, RoomVersionId,
	ServerName, SigningKeyAlgorithm, UserId,
	api::{
		error::{Error as RumaError, ErrorBody, ErrorKind, RetryAfter, StandardErrorBody},
		federation::policy::sign_event::v1::Request as SignEventRequest,
	},
	canonical_json::redact,
	events::{
		StateEventType, TimelineEventType,
		room::{
			member::MembershipState,
			policy::{POLICY_SERVER_ED25519_SIGNING_KEY_ID, RoomPolicyEventContent},
		},
	},
	room_version_rules::RoomVersionRules,
	serde::{Base64, base64::Standard},
	signatures::{to_canonical_json_string_for_signing, verify_canonical_json_bytes},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, value::to_raw_value};
use tokio::time::timeout;
use tuwunel_core::{
	Error, Result, at,
	config::rooms::PolicyServer,
	debug, err, implement, trace,
	utils::{BoolExt as _, time::now_secs},
	warn,
};
use tuwunel_database::{Cbor, Deserialized};
use tuwunel_matrix::{
	Event, pdu::into_outgoing_federation, room_version::rules as room_version_rules,
};

use crate::federation::{Classification, ShouldAttempt, is_local_refusal};

#[cfg(test)]
mod tests;

/// MSC4284 unstable state event type. The merged spec stabilised this to
/// `m.room.policy`, but the reference policy server (and Element's default
/// deployments as of 2026-05) still write the unstable type with the singular
/// `public_key` field; reading both keeps the gate live for those rooms.
const UNSTABLE_POLICY_TYPE: &str = "org.matrix.msc4284.policy";
const POLICY_REFUSAL_TTL: Duration = Duration::from_hours(24);

/// Outcome of an inbound policy-server signature check.
#[derive(Clone, Copy, Debug)]
pub enum PolicyCheck {
	/// No policy server is configured for this room (or feature is off, or
	/// the event is the policy state event itself). The caller should not
	/// modify its soft-fail decision based on policy considerations.
	NotApplicable,

	/// Policy server signature is present and verifies cleanly.
	Pass,

	/// Policy server signature is absent. Per MSC4284, the homeserver SHOULD
	/// either fetch one from the policy server or soft-fail.
	Missing,

	/// The policy server signature failed verification, or the policy server
	/// refused the event. Soft-fail.
	Invalid,

	/// No valid signature could be obtained and the policy server gave no
	/// verdict, which `fail_open` does not tolerate. Soft-fail an event received
	/// live; backfill defers one until it is offered again.
	NoVerdict,
}

/// The policy server that signs a room's local events, with the room's
/// version.
///
/// Only `outgoing_policy` makes one, once the event passes the gates deciding
/// whether a policy server applies to it. Two are equal when they name the same
/// policy server and keys in the same room version.
#[derive(Debug)]
pub struct OutgoingPolicy {
	policy: RoomPolicyEventContent,
	room_version: RoomVersionId,
}

/// Outcome of a `/sign` round-trip to the policy server.
#[derive(Debug)]
enum FetchOutcome {
	/// Policy server returned a signature that verifies.
	Signed(String),

	/// Policy server explicitly refused the event.
	///
	/// A 400 `M_FORBIDDEN` is ambiguous because policyserv also uses it for
	/// unknown rooms and invalid origin signatures. A 200 without a signature
	/// for `via` is the unstable refusal form and carries no error.
	Refused(Option<RumaError>),

	/// Policy server gave no verdict on the event.
	NoVerdict(NoVerdict),
}

/// Why the policy server gave no verdict on an event.
///
/// Every cause rejects the event unless a missing verdict is tolerated; the
/// cause decides the error the client sees.
#[derive(Debug)]
enum NoVerdict {
	/// It answered with an error other than a refusal.
	Failed(RumaError),

	/// This server's configuration forbids contacting it.
	NotPermitted,

	/// Its signature failed verification against the room's policy key.
	Unverified,

	/// No answer: a timeout, a transport failure, a backoff, or a response
	/// that is not a Matrix error.
	Unavailable,
}

/// How a member event takes a member out of the room.
///
/// Only another member's leave needs the target's current membership, to tell
/// a kick from lifting a ban.
#[derive(Debug, Eq, PartialEq)]
enum Removal<'a> {
	/// The member's own leave, which also rejects an invite or withdraws a
	/// knock.
	Leave,

	/// A ban.
	Ban,

	/// Another member's leave: a kick, unless it lifts a ban.
	KickOrUnban(&'a UserId),
}

/// Persisted per-event policy-server refusal in `eventid_policysigstate`.
///
/// Absence of a row means "no prior decision recorded; proceed with `/sign`".
#[derive(Debug, Serialize, Deserialize)]
enum PolicySigState {
	/// Policy server refused this event.
	///
	/// Requests wait until this unix-secs deadline before retrying.
	Refused {
		until_secs: u64,
	},
}

/// Lenient deserialiser that accepts either the stable
/// `public_keys: { ed25519: ... }` shape or the MSC4284 unstable singular
/// `public_key: <ed25519>` shape, and folds the latter into the former.
#[derive(Deserialize)]
struct UnstablePolicyContent {
	via: OwnedServerName,

	#[serde(default)]
	public_keys: BTreeMap<SigningKeyAlgorithm, Base64>,

	#[serde(default)]
	public_key: Option<Base64>,
}

#[implement(UnstablePolicyContent)]
fn into_stable(
	Self { via, mut public_keys, public_key }: Self,
) -> Option<RoomPolicyEventContent> {
	if let Some(key) = public_key {
		public_keys
			.entry(SigningKeyAlgorithm::Ed25519)
			.or_insert(key);
	}

	let ed25519 = public_keys.remove(&SigningKeyAlgorithm::Ed25519)?;

	Some(RoomPolicyEventContent::new(via, ed25519))
}

impl PartialEq for OutgoingPolicy {
	fn eq(&self, other: &Self) -> bool {
		self.policy.via == other.policy.via
			&& self.policy.public_keys == other.policy.public_keys
			&& self.room_version == other.room_version
	}
}

/// Clears the cached policy server decision for an event.
///
/// A later validation can contact the policy server again.
#[implement(super::Service)]
pub fn clear_policy_signature_state(&self, event_id: &EventId) {
	self.db.eventid_policysigstate.remove(event_id);
}

#[implement(super::Service)]
fn cache_policy_refused(&self, event_id: &EventId) {
	let until_secs = now_secs().saturating_add(POLICY_REFUSAL_TTL.as_secs());

	self.db
		.eventid_policysigstate
		.raw_put(event_id.as_str(), Cbor(&PolicySigState::Refused { until_secs }));
}

#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip(self))]
async fn refusal_cached(&self, event_id: &EventId) -> bool {
	let state = self
		.db
		.eventid_policysigstate
		.get(event_id.as_str())
		.await
		.deserialized::<Cbor<_>>();

	current_policy_state(state, now_secs()).is_some()
}

fn current_policy_state(
	state: Result<Cbor<PolicySigState>>,
	current_secs: u64,
) -> Option<PolicySigState> {
	state
		.map(at!(0))
		.ok()
		.filter(|&PolicySigState::Refused { until_secs }| until_secs > current_secs)
}

/// Returns the room's policy event content when a policy server is in effect:
/// state event present (stable `m.room.policy`, falling back to MSC4284's
/// unstable `org.matrix.msc4284.policy`), parses cleanly under either the
/// stable `public_keys` map or the unstable singular `public_key` field, and
/// the `via` server has a joined user in the room. Any failure returns `None`,
/// signalling "no policy server configured" so the caller skips the gate
/// entirely.
#[implement(super::Service)]
pub async fn lookup_policy_server(&self, room_id: &RoomId) -> Option<RoomPolicyEventContent> {
	let read = async |event_type: &StateEventType| {
		self.services
			.state_accessor
			.room_state_get_content::<UnstablePolicyContent>(room_id, event_type, "")
			.await
			.ok()
			.and_then(UnstablePolicyContent::into_stable)
	};

	let content = match read(&StateEventType::RoomPolicy).await {
		| Some(content) => content,
		| None => read(&StateEventType::from(UNSTABLE_POLICY_TYPE.to_owned())).await?,
	};

	self.services
		.state_cache
		.server_in_room(&content.via, room_id)
		.await
		.then_some(content)
}

/// MSC4284: ask the room's policy server to sign an outgoing event.
///
/// An event no policy server applies to goes out unsigned; any other is signed
/// as `sign_outgoing_with` does.
#[implement(super::Service)]
#[tracing::instrument(name = "policy_sign_outgoing", level = "debug", skip_all)]
pub async fn sign_outgoing_pdu<E>(
	&self,
	pdu_json: CanonicalJsonObject,
	pdu: &E,
) -> Result<CanonicalJsonObject>
where
	E: Event,
{
	let Some(outgoing) = self
		.outgoing_policy(pdu.room_id(), pdu.kind(), pdu.state_key())
		.await
	else {
		return Ok(pdu_json);
	};

	self.sign_outgoing_with(&outgoing, pdu_json, pdu)
		.await
}

/// The room's policy server, when it signs a local event of this type.
///
/// There is none when this server takes no part in policy servers, the event
/// is the policy state event itself, or the room has no usable policy server.
/// Only the type and state key are asked for, so a caller can decide before it
/// builds the event.
#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip(self))]
pub async fn outgoing_policy(
	&self,
	room_id: &RoomId,
	kind: &TimelineEventType,
	state_key: Option<&str>,
) -> Option<OutgoingPolicy> {
	if self.policy_servers_apply().is_false() || is_policy_state_event(kind, state_key) {
		return None;
	}

	let Some(policy) = self.lookup_policy_server(room_id).await else {
		trace!(%room_id, "no policy server configured");
		return None;
	};

	let room_version = self
		.services
		.state
		.get_room_version(room_id)
		.await
		.ok()?;

	Some(OutgoingPolicy { policy, room_version })
}

/// MSC4284: ask a room's policy server to sign an outgoing event.
///
/// A valid signature is folded into `pdu_json["signatures"]` so it persists
/// with the event and federates to the other servers in the room. A refusal
/// fails the request with the policy server's own error. When the policy server
/// gives no verdict the request fails too, unless `fail_open` is set or the
/// event is a leave, a kick or a ban.
#[implement(super::Service)]
#[tracing::instrument(name = "policy_sign", level = "debug", skip_all)]
pub async fn sign_outgoing_with<E>(
	&self,
	outgoing: &OutgoingPolicy,
	pdu_json: CanonicalJsonObject,
	pdu: &E,
) -> Result<CanonicalJsonObject>
where
	E: Event,
{
	let signature = self
		.outbound_verdict(outgoing, &pdu_json, pdu)
		.await?;

	let pdu_json = signature
		.into_iter()
		.fold(pdu_json, |pdu_json, signature| {
			folded(pdu_json, &outgoing.policy.via, signature)
		});

	Ok(pdu_json)
}

/// The policy server's signature to fold into a local event, or `None` to send
/// it without one.
///
/// A refusal, and a missing verdict the event may not go out without, are the
/// error failing the client's request.
#[implement(super::Service)]
async fn outbound_verdict<E>(
	&self,
	OutgoingPolicy { policy, room_version }: &OutgoingPolicy,
	pdu_json: &CanonicalJsonObject,
	pdu: &E,
) -> Result<Option<String>>
where
	E: Event,
{
	let event_id = pdu.event_id();

	if self.refusal_cached(event_id).await {
		return Err(refusal_error(None));
	}

	let outcome = self
		.fetch_policy_signature(policy, pdu_json, room_version)
		.await;

	if let FetchOutcome::Refused(remote) = &outcome {
		warn!(
			via = %policy.via,
			%event_id,
			room_id = %pdu.room_id(),
			?remote,
			"policy server refused to sign outbound PDU"
		);

		self.cache_policy_refused(event_id);
	}

	let tolerated = matches!(outcome, FetchOutcome::NoVerdict(_))
		&& self.tolerates_missing_verdict(pdu).await;

	let Some(signature) = outbound_signature(outcome, tolerated, &policy.via)? else {
		debug!(via = %policy.via, %event_id, "sending without a policy server signature");
		return Ok(None);
	};

	debug!(via = %policy.via, %event_id, "folding policy server signature");

	Ok(Some(signature))
}

fn folded(
	mut pdu_json: CanonicalJsonObject,
	via: &ServerName,
	signature: String,
) -> CanonicalJsonObject {
	insert_policy_signature(&mut pdu_json, via, signature);
	pdu_json
}

#[implement(super::Service)]
#[inline]
fn policy_servers_apply(&self) -> bool {
	takes_part(&self.services.server.config.rooms.policy_server)
}

/// Whether this server takes part in room policy servers.
///
/// No supported room version makes the policy server's signature an
/// authorization rule, so `advisory_room_versions` reaches every room.
fn takes_part(config: &PolicyServer) -> bool { config.enable && config.advisory_room_versions }

/// The client's error for a refusal.
///
/// That is the policy server's own answer, or a generic refusal when it refused
/// by withholding its signature.
fn refusal_error(remote: Option<RumaError>) -> Error {
	remote.map(relayed).unwrap_or_else(|| {
		err!(Request(Forbidden("Event was rejected by the room's policy server.")))
	})
}

/// Relays a policy server's error answer to the client.
///
/// The answer goes out verbatim, except where it would speak for the user's
/// own login or account: a 401 goes out as 403, and an errcode describing the
/// user's session, account or homeserver as 403 `M_FORBIDDEN`, keeping only
/// the policy server's message.
fn relayed(remote: RumaError) -> Error {
	let status = if remote.status_code == StatusCode::UNAUTHORIZED {
		StatusCode::FORBIDDEN
	} else {
		remote.status_code
	};

	let error = match remote.body {
		| ErrorBody::Standard(StandardErrorBody { kind, message })
			if describes_client_session(&kind) =>
		{
			let body = StandardErrorBody::new(ErrorKind::forbidden(), message);

			RumaError::new(StatusCode::FORBIDDEN, ErrorBody::Standard(body))
		},
		| body => RumaError::new(status, body),
	};

	Error::Ruma(error)
}

/// Whether an errcode speaks of the user's own session, account or homeserver.
///
/// These describe the user's own session, account or homeserver, which a third
/// party cannot speak for.
fn describes_client_session(kind: &ErrorKind) -> bool {
	matches!(
		kind,
		ErrorKind::UnknownToken(_)
			| ErrorKind::MissingToken
			| ErrorKind::UserDeactivated
			| ErrorKind::UserLocked
			| ErrorKind::UserSuspended
			| ErrorKind::ResourceLimitExceeded(_)
	) || kind.errcode().as_str() == "M_CONSENT_NOT_GIVEN"
}

/// Asks the room's policy server to sign an event, unless it is backed off.
///
/// The answer is classified for each caller to react to; a returned signature
/// is verified against the room's policy key first, so only a valid one is
/// `Signed`.
#[implement(super::Service)]
#[tracing::instrument(
	name = "policy_fetch",
	level = "debug",
	skip_all,
	fields(via = %policy.via)
)]
async fn fetch_policy_signature(
	&self,
	policy: &RoomPolicyEventContent,
	pdu_json: &CanonicalJsonObject,
	room_version: &RoomVersionId,
) -> FetchOutcome {
	let via = &policy.via;
	let attempt = self.services.federation.should_attempt(via).await;

	if let ShouldAttempt::No { earliest_retry } = attempt {
		debug!(%via, ?earliest_retry, "policy server is backed off; skipping /sign");
		return FetchOutcome::NoVerdict(NoVerdict::Unavailable);
	}

	let outgoing = into_outgoing_federation(pdu_json.clone(), room_version);
	let Ok(raw) = to_raw_value(&outgoing) else {
		warn!(%via, "failed to serialize PDU for policy /sign");
		return FetchOutcome::NoVerdict(NoVerdict::Unavailable);
	};

	let timeout_secs = self
		.services
		.server
		.config
		.rooms
		.policy_server
		.request_timeout;

	let limit = Duration::from_secs(timeout_secs);

	let request = self
		.services
		.federation
		.execute_allow_self(via, SignEventRequest::new(raw));

	match timeout(limit, request).await {
		| Ok(Err(error)) => self.failed_fetch(via, error),
		| Ok(Ok(response)) => {
			let signature = response.ed25519_signature(via);
			let outcome = signature_outcome(pdu_json, room_version, policy, signature);

			if matches!(outcome, FetchOutcome::NoVerdict(NoVerdict::Unverified)) {
				warn!(%via, "policy server signature fails verification");
			}

			outcome
		},
		| Err(elapsed) => {
			warn!(%via, %elapsed, "policy server /sign timed out");

			// The abandoned request never reached its own outcome record.
			self.services
				.federation
				.record_failure(via, Classification::Transient);

			FetchOutcome::NoVerdict(NoVerdict::Unavailable)
		},
	}
}

/// Classifies a policy server's 200 answer by the signature it carries.
///
/// No signature for `via` is the MSC4284 unstable form of a refusal. A
/// signature failing verification against the room's policy key is no verdict,
/// and never reaches the event.
fn signature_outcome(
	pdu_json: &CanonicalJsonObject,
	room_version: &RoomVersionId,
	policy: &RoomPolicyEventContent,
	signature: Option<&str>,
) -> FetchOutcome {
	let Some(signature) = signature else {
		return FetchOutcome::Refused(None);
	};

	let verified = policy
		.public_keys
		.get(&SigningKeyAlgorithm::Ed25519)
		.zip(room_version_rules(room_version).ok())
		.is_some_and(|(public_key, rules)| {
			verify_policy_signature(pdu_json, room_version, &rules, signature, public_key)
		});

	if verified {
		FetchOutcome::Signed(signature.to_owned())
	} else {
		FetchOutcome::NoVerdict(NoVerdict::Unverified)
	}
}

/// Verifies a policy server's signature on an event.
///
/// The policy server signed the event as federation carries it, so fields only
/// our stored copy holds (the `event_id` of later room versions) are dropped
/// before checking.
fn verify_policy_signature(
	pdu_json: &CanonicalJsonObject,
	room_version: &RoomVersionId,
	rules: &RoomVersionRules,
	signature: &str,
	public_key: &Base64,
) -> bool {
	let Ok(signature) = Base64::<Standard>::parse(signature) else {
		return false;
	};

	let outgoing = into_outgoing_federation(pdu_json.clone(), room_version);
	let Ok(redacted) = redact(outgoing, &rules.redaction, None) else {
		return false;
	};

	let Ok(canonical) = to_canonical_json_string_for_signing(&redacted) else {
		return false;
	};

	verify_canonical_json_bytes(
		&SigningKeyAlgorithm::Ed25519,
		public_key.as_bytes(),
		signature.as_bytes(),
		canonical.as_bytes(),
	)
	.inspect_err(|error| debug!(%error, "policy server signature failed verification"))
	.is_ok()
}

/// Logs and classifies a failed `/sign` request.
///
/// A deadline a rate-limiting policy server names is recorded as its backoff.
/// A refusal is left to the caller to log with the event it concerns.
#[implement(super::Service)]
fn failed_fetch(&self, via: &ServerName, error: Error) -> FetchOutcome {
	let error = standardized(error);

	if let Error::Federation(_, remote) = &error
		&& let Some(retry_until) = retry_until(remote, now_secs())
	{
		self.services
			.federation
			.record_retry_after(via, retry_until);
	}

	match &error {
		| Error::Federation(_, remote) if is_refusal(remote) => {},
		| Error::Federation(_, remote) if is_unsupported(remote) =>
			debug!(%via, %error, "policy server does not support /sign"),
		| _ if is_local_refusal(&error) => warn!(
			%via,
			%error,
			"this server's configuration forbids contacting the room's policy server"
		),
		| _ => warn!(%via, %error, "policy server /sign failed"),
	}

	classify_fetch_error(error)
}

/// Sorts a failed `/sign` request by what answered it.
///
/// A Matrix error from the policy server is a refusal when it is 400
/// `M_FORBIDDEN` and a failure otherwise; this server's own configuration
/// refusing the request is kept apart; anything else is no answer at all.
fn classify_fetch_error(error: Error) -> FetchOutcome {
	match error {
		| Error::Federation(_, remote) if is_refusal(&remote) =>
			FetchOutcome::Refused(Some(remote)),
		| Error::Federation(_, remote) if remote.error_kind().is_some() =>
			FetchOutcome::NoVerdict(NoVerdict::Failed(remote)),
		| error if is_local_refusal(&error) => FetchOutcome::NoVerdict(NoVerdict::NotPermitted),
		| _ => FetchOutcome::NoVerdict(NoVerdict::Unavailable),
	}
}

/// Gives a policy server's message-less Matrix error a standard body.
///
/// An error's `error` message is optional, but a body without one does not parse
/// as a standard error, and would otherwise read as no Matrix error at all.
fn standardized(error: Error) -> Error {
	match error {
		| Error::Federation(origin, remote) => Error::Federation(origin, standard_body(remote)),
		| error => error,
	}
}

fn standard_body(remote: RumaError) -> RumaError {
	let ErrorBody::Json(json) = &remote.body else {
		return remote;
	};

	let Ok(kind) = ErrorKind::deserialize(json) else {
		return remote;
	};

	let message = json
		.get("error")
		.and_then(JsonValue::as_str)
		.unwrap_or_default()
		.to_owned();

	let body = StandardErrorBody::new(kind, message);

	RumaError::new(remote.status_code, ErrorBody::Standard(body))
}

fn is_refusal(remote: &RumaError) -> bool {
	remote.status_code == StatusCode::BAD_REQUEST
		&& matches!(remote.error_kind(), Some(ErrorKind::Forbidden))
}

fn is_unsupported(remote: &RumaError) -> bool {
	remote.status_code == StatusCode::NOT_FOUND
		&& matches!(remote.error_kind(), Some(ErrorKind::NotFound | ErrorKind::Unrecognized))
}

/// The deadline a rate-limiting policy server asked for, in seconds since the
/// epoch.
///
/// A delay counts from `now`; a date is taken as given.
fn retry_until(remote: &RumaError, now: u64) -> Option<u64> {
	let Some(ErrorKind::LimitExceeded(limit)) = remote.error_kind() else {
		return None;
	};

	match limit.retry_after.as_ref()? {
		| RetryAfter::Delay(delay) => Some(now.saturating_add(delay.as_secs())),
		| RetryAfter::DateTime(time) => time
			.duration_since(UNIX_EPOCH)
			.ok()
			.as_ref()
			.map(Duration::as_secs),
	}
}

/// Whether a local event goes out when the policy server gave no verdict.
///
/// The operator may allow it with `fail_open`. A leave, a kick or a ban always
/// goes out, so an outage never traps users in a room.
#[implement(super::Service)]
async fn tolerates_missing_verdict<E>(&self, pdu: &E) -> bool
where
	E: Event,
{
	self.services
		.server
		.config
		.rooms
		.policy_server
		.fail_open
		|| self.reduces_access(pdu).await
}

/// Whether a member event takes access to the room away: the member's own
/// leave, a kick or a ban.
///
/// Another member's leave counts only when the target's current membership is
/// known to give access; that alone needs a state read.
#[implement(super::Service)]
async fn reduces_access<E>(&self, pdu: &E) -> bool
where
	E: Event,
{
	match removal(pdu) {
		| None => false,
		| Some(Removal::Leave | Removal::Ban) => true,
		| Some(Removal::KickOrUnban(target)) => {
			let membership = self
				.services
				.state_accessor
				.get_member(pdu.room_id(), target)
				.await
				.map(|member| member.membership);

			is_kick(membership)
		},
	}
}

/// Whether another member's leave is a kick, given the target's current
/// membership.
///
/// It is when the target is joined, invited or knocking. Lifting a ban grants
/// access, and an unreadable membership proves nothing, so neither counts.
fn is_kick(membership: Result<MembershipState>) -> bool {
	membership.is_ok_and(|membership| {
		matches!(
			membership,
			MembershipState::Join | MembershipState::Invite | MembershipState::Knock
		)
	})
}

fn removal<E: Event>(pdu: &E) -> Option<Removal<'_>> {
	let target = pdu
		.state_key()
		.and_then(|key| <&UserId>::try_from(key).ok())?;

	match pdu.membership_for(target)? {
		| MembershipState::Ban => Some(Removal::Ban),
		| MembershipState::Leave if target == pdu.sender() => Some(Removal::Leave),
		| MembershipState::Leave => Some(Removal::KickOrUnban(target)),
		| _ => None,
	}
}

/// Decides a local event from the policy server's outcome.
///
/// Returns the signature to fold in, `None` to send the event without one, or
/// the error failing the client's request. `tolerated` says whether a missing
/// verdict lets the event out; a refusal never does.
fn outbound_signature(
	outcome: FetchOutcome,
	tolerated: bool,
	via: &ServerName,
) -> Result<Option<String>> {
	match outcome {
		| FetchOutcome::Signed(signature) => Ok(Some(signature)),
		| FetchOutcome::Refused(remote) => Err(refusal_error(remote)),
		| FetchOutcome::NoVerdict(_) if tolerated => Ok(None),
		| FetchOutcome::NoVerdict(cause) => Err(cause.into_error(via)),
	}
}

/// The client's error for a local event refused for want of a verdict.
///
/// A policy server's error answer is relayed, this server's own configuration
/// refusing is a 403, and no usable answer is a 503 the client may retry.
#[implement(NoVerdict)]
fn into_error(self, via: &ServerName) -> Error {
	match self {
		| Self::Failed(remote) => relayed(remote),
		| Self::NotPermitted => err!(Request(Forbidden(
			"This server's configuration does not permit contacting the room's policy server."
		))),
		| Self::Unverified | Self::Unavailable => err!(RequestStatus(
			SERVICE_UNAVAILABLE,
			Unknown("The room's policy server {via} is unavailable.")
		)),
	}
}

/// MSC4284: verify the inbound PDU's policy server signature.
///
/// Missing or invalid signatures are fetched again because the policy server's
/// key may have rotated. A fetched signature is folded into the event.
#[implement(super::Service)]
#[tracing::instrument(name = "policy_verify_or_fetch", level = "debug", skip_all)]
pub async fn verify_or_fetch_inbound_policy_signature<E>(
	&self,
	pdu_json: &mut CanonicalJsonObject,
	pdu: &E,
) -> PolicyCheck
where
	E: Event,
{
	match self
		.check_inbound_policy_signature(pdu_json, pdu)
		.await
	{
		| PolicyCheck::Missing | PolicyCheck::Invalid =>
			self.fetch_inbound_policy_signature(pdu_json, pdu)
				.await,
		| other => other,
	}
}

/// MSC4284: fetch a missing or invalid inbound policy server signature.
///
/// A valid signature is folded into `pdu_json` so it persists and federates
/// onward. A refusal maps to `Invalid`, and a missing verdict to `NoVerdict`
/// unless `fail_open` is set.
#[implement(super::Service)]
#[tracing::instrument(name = "policy_fetch_inbound", level = "debug", skip_all)]
async fn fetch_inbound_policy_signature<E>(
	&self,
	pdu_json: &mut CanonicalJsonObject,
	pdu: &E,
) -> PolicyCheck
where
	E: Event,
{
	let Some(policy) = self.lookup_policy_server(pdu.room_id()).await else {
		return PolicyCheck::NotApplicable;
	};

	let Ok(room_version) = self
		.services
		.state
		.get_room_version(pdu.room_id())
		.await
	else {
		return PolicyCheck::NotApplicable;
	};

	let event_id = pdu.event_id();

	if self.refusal_cached(event_id).await {
		self.settle_and_store(event_id, pdu_json, &policy.via, FetchOutcome::Refused(None));
		return PolicyCheck::Invalid;
	}

	let outcome = self
		.fetch_policy_signature(&policy, pdu_json, &room_version)
		.await;

	let fail_open = self
		.services
		.server
		.config
		.rooms
		.policy_server
		.fail_open;

	let check = inbound_check(&outcome, fail_open);

	match &outcome {
		| FetchOutcome::NoVerdict(_) => {},
		| FetchOutcome::Signed(_) => {
			debug!(via = %policy.via, %event_id, "folded inbound policy server signature");
		},
		| FetchOutcome::Refused(remote) => {
			warn!(
				via = %policy.via,
				%event_id,
				room_id = %pdu.room_id(),
				?remote,
				"policy server refused to sign inbound PDU; soft-failing"
			);

			self.cache_policy_refused(event_id);
		},
	}

	self.settle_and_store(event_id, pdu_json, &policy.via, outcome);

	check
}

/// Settles an incoming event's policy signature and stores the result.
///
/// The event is already stored as an outlier, and that copy is what is served
/// while it is soft-failed, so a changed signature is written back to it.
#[implement(super::Service)]
fn settle_and_store(
	&self,
	event_id: &EventId,
	pdu_json: &mut CanonicalJsonObject,
	via: &ServerName,
	outcome: FetchOutcome,
) {
	if settle_signature(pdu_json, via, outcome) {
		self.services
			.timeline
			.add_pdu_outlier(event_id, pdu_json);
	}
}

/// Settles an incoming event's policy signature after asking for a new one,
/// returning whether the event changed.
///
/// A valid signature replaces the event's own; any other outcome strips the
/// event's own, since it was missing or failed verification.
fn settle_signature(
	pdu_json: &mut CanonicalJsonObject,
	via: &ServerName,
	outcome: FetchOutcome,
) -> bool {
	match outcome {
		| FetchOutcome::Refused(_) | FetchOutcome::NoVerdict(_) =>
			remove_policy_signature(pdu_json, via),
		| FetchOutcome::Signed(signature) => {
			insert_policy_signature(pdu_json, via, signature);
			true
		},
	}
}

/// Maps the policy server's outcome on an incoming event to its check.
///
/// A missing verdict passes only when `fail_open` is set.
fn inbound_check(outcome: &FetchOutcome, fail_open: bool) -> PolicyCheck {
	match outcome {
		| FetchOutcome::Signed(_) => PolicyCheck::Pass,
		| FetchOutcome::Refused(_) => PolicyCheck::Invalid,
		| FetchOutcome::NoVerdict(_) if fail_open => PolicyCheck::Pass,
		| FetchOutcome::NoVerdict(_) => PolicyCheck::NoVerdict,
	}
}

/// Whether an event received live is soft-failed on this check.
///
/// Backfill tells a refusal from a missing verdict instead, deferring the
/// latter until the event is offered again.
#[implement(PolicyCheck)]
pub(super) fn soft_fails(self) -> bool { matches!(self, Self::Invalid | Self::NoVerdict) }

/// MSC4284: verify the policy server signature on an inbound PDU. Returns
/// `NotApplicable` for rooms without a configured policy server (the gate is
/// skipped); `Pass` when the signature verifies; `Missing` when no signature
/// is present for the configured server; `Invalid` when the signature is
/// present but cryptographic verification fails.
#[implement(super::Service)]
#[tracing::instrument(name = "policy_verify", level = "debug", skip_all)]
pub async fn check_inbound_policy_signature<E>(
	&self,
	pdu_json: &CanonicalJsonObject,
	pdu: &E,
) -> PolicyCheck
where
	E: Event,
{
	if self.policy_servers_apply().is_false()
		|| is_policy_state_event(pdu.kind(), pdu.state_key())
	{
		return PolicyCheck::NotApplicable;
	}

	let Some(policy) = self.lookup_policy_server(pdu.room_id()).await else {
		return PolicyCheck::NotApplicable;
	};

	let Ok(room_version) = self
		.services
		.state
		.get_room_version(pdu.room_id())
		.await
	else {
		return PolicyCheck::NotApplicable;
	};

	// `lookup_policy_server` already verified the ed25519 entry is present.
	let Some(public_key) = policy
		.public_keys
		.get(&SigningKeyAlgorithm::Ed25519)
	else {
		return PolicyCheck::NotApplicable;
	};

	check_policy_signature(pdu_json, &room_version, &policy.via, public_key)
}

fn check_policy_signature(
	pdu_json: &CanonicalJsonObject,
	room_version: &RoomVersionId,
	via: &ServerName,
	public_key: &Base64,
) -> PolicyCheck {
	let Ok(rules) = room_version_rules(room_version) else {
		return PolicyCheck::NotApplicable;
	};

	let Some(signature) = extract_policy_signature(pdu_json, via) else {
		return PolicyCheck::Missing;
	};

	if verify_policy_signature(pdu_json, room_version, &rules, signature, public_key) {
		PolicyCheck::Pass
	} else {
		PolicyCheck::Invalid
	}
}

fn is_policy_state_event(kind: &TimelineEventType, state_key: Option<&str>) -> bool {
	if state_key != Some("") {
		return false;
	}

	let kind = kind.to_cow_str();

	kind == "m.room.policy" || kind == UNSTABLE_POLICY_TYPE
}

fn extract_policy_signature<'a>(
	pdu_json: &'a CanonicalJsonObject,
	via: &ServerName,
) -> Option<&'a str> {
	let CanonicalJsonValue::Object(servers) = pdu_json.get("signatures")? else {
		return None;
	};

	let CanonicalJsonValue::Object(keys) = servers.get(via.as_str())? else {
		return None;
	};

	let CanonicalJsonValue::String(signature) = keys.get(POLICY_SERVER_ED25519_SIGNING_KEY_ID)?
	else {
		return None;
	};

	Some(signature.as_str())
}

fn insert_policy_signature(
	pdu_json: &mut CanonicalJsonObject,
	via: &ServerName,
	signature: String,
) {
	let signatures = pdu_json
		.entry("signatures".into())
		.or_insert_with(|| CanonicalJsonValue::Object(BTreeMap::new()));

	let CanonicalJsonValue::Object(servers) = signatures else {
		return;
	};

	let entry = servers
		.entry(via.as_str().into())
		.or_insert_with(|| CanonicalJsonValue::Object(BTreeMap::new()));

	if let CanonicalJsonValue::Object(keys) = entry {
		keys.insert(
			POLICY_SERVER_ED25519_SIGNING_KEY_ID.into(),
			CanonicalJsonValue::String(signature),
		);
	}
}

/// Removes the policy server's signature from an event, returning whether it
/// had one.
///
/// Other signatures under the same server name, the origin's own when the
/// policy server shares it, are kept.
fn remove_policy_signature(pdu_json: &mut CanonicalJsonObject, via: &ServerName) -> bool {
	let Some(CanonicalJsonValue::Object(servers)) = pdu_json.get_mut("signatures") else {
		return false;
	};

	let Some(CanonicalJsonValue::Object(keys)) = servers.get_mut(via.as_str()) else {
		return false;
	};

	let removed = keys
		.remove(POLICY_SERVER_ED25519_SIGNING_KEY_ID)
		.is_some();

	if keys.is_empty() {
		servers.remove(via.as_str());
	}

	removed
}
