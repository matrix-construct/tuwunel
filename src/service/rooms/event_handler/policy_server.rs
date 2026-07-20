use std::{
	collections::BTreeMap,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedServerName, RoomId, RoomVersionId,
	ServerName, SigningKeyAlgorithm,
	api::{
		error::{ErrorKind, RetryAfter},
		federation::policy::sign_event::v1 as sign_event,
	},
	events::{
		StateEventType,
		room::policy::{POLICY_SERVER_ED25519_SIGNING_KEY_ID, RoomPolicyEventContent},
	},
	serde::Base64,
	signatures::{to_canonical_json_string_for_signing, verify_canonical_json_bytes},
};
use serde::{Deserialize, Serialize};
use serde_json::value::to_raw_value;
use tuwunel_core::{
	Err, Result, at, debug, implement,
	matrix::{Event, pdu::into_outgoing_federation, room_version},
	trace,
	utils::time::now_secs,
	warn,
};
use tuwunel_database::{Cbor, Deserialized};

/// MSC4284 unstable state event type. The merged spec stabilised this to
/// `m.room.policy`, but the reference policy server (and Element's default
/// deployments as of 2026-05) still write the unstable type with the singular
/// `public_key` field; reading both keeps the gate live for those rooms.
const UNSTABLE_POLICY_TYPE: &str = "org.matrix.msc4284.policy";

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

	/// Policy server signature is present but failed cryptographic
	/// verification. Soft-fail.
	Invalid,
}

/// Outcome of a `/sign` round-trip to the policy server.
#[derive(Debug)]
enum FetchOutcome {
	/// Policy server returned a valid signature.
	Signed(String),

	/// Network error or timeout; the caller should fail open.
	FailOpen,

	/// Policy server explicitly refused (Forbidden), or returned 200 OK
	/// with no signature for our `via` (the MSC4284 unstable refusal).
	Refused,

	/// Policy server returned `M_LIMIT_EXCEEDED`. The caller should record
	/// the unix-secs deadline so subsequent attempts before then are
	/// short-circuited.
	RateLimited {
		until_secs: u64,
	},
}

/// Persisted per-event policy-server outcome in `eventid_policysigstate`.
/// Absence of a row means "no prior decision recorded; proceed with `/sign`".
#[derive(Debug, Serialize, Deserialize)]
enum PolicySigState {
	/// Policy server already refused this event; do not retry.
	Refused,

	/// Policy server is rate-limiting; do not retry before this unix-secs
	/// deadline.
	BackoffUntil {
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

#[implement(super::Service)]
fn cache_policy_refused(&self, event_id: &EventId) {
	self.db
		.eventid_policysigstate
		.raw_put(event_id.as_str(), Cbor(&PolicySigState::Refused));
}

#[implement(super::Service)]
fn cache_policy_backoff(&self, event_id: &EventId, until_secs: u64) {
	self.db
		.eventid_policysigstate
		.raw_put(event_id.as_str(), Cbor(&PolicySigState::BackoffUntil { until_secs }));
}

#[implement(super::Service)]
async fn cached_policy_state(&self, event_id: &EventId) -> Option<PolicySigState> {
	self.db
		.eventid_policysigstate
		.get(event_id.as_str())
		.await
		.deserialized::<Cbor<_>>()
		.map(at!(0))
		.ok()
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

/// MSC4284: ask the room's policy server to sign an outgoing event. The
/// signature is folded into `pdu_json["signatures"]` so it persists with the
/// event and federates transitively to other servers in the room. Returns
/// `Forbidden` when the policy server explicitly refuses; network errors and
/// timeouts fail open with a warn log.
#[implement(super::Service)]
#[tracing::instrument(name = "policy_sign", level = "debug", skip_all)]
pub async fn sign_outgoing_pdu<E>(&self, pdu_json: &mut CanonicalJsonObject, pdu: &E) -> Result
where
	E: Event,
{
	if !self.services.server.config.enable_policy_servers {
		return Ok(());
	}

	if is_policy_state_event(pdu) {
		return Ok(());
	}

	let Ok(room_version) = self
		.services
		.state
		.get_room_version(pdu.room_id())
		.await
	else {
		return Ok(());
	};

	let Some(policy) = self.lookup_policy_server(pdu.room_id()).await else {
		trace!(room_id = %pdu.room_id(), "no policy server configured");
		return Ok(());
	};

	let event_id = pdu.event_id();
	match self.cached_policy_state(event_id).await {
		| Some(PolicySigState::Refused) =>
			return Err!(Request(Forbidden("Event was rejected by the room's policy server."))),

		| Some(PolicySigState::BackoffUntil { until_secs }) if until_secs > now_secs() => {
			debug!(via = %policy.via, until_secs, "skipping outbound /sign during policy backoff");
			return Ok(());
		},
		| _ => {},
	}

	match self
		.fetch_policy_signature(&policy, pdu_json, &room_version)
		.await
	{
		| FetchOutcome::Signed(signature) => {
			insert_policy_signature(pdu_json, &policy.via, &signature);
			debug!(via = %policy.via, event_id = %event_id, "folded policy server signature");
		},
		| FetchOutcome::Refused => {
			self.cache_policy_refused(event_id);
			return Err!(Request(Forbidden("Event was rejected by the room's policy server.")));
		},
		| FetchOutcome::RateLimited { until_secs } => {
			self.cache_policy_backoff(event_id, until_secs);
		},
		| FetchOutcome::FailOpen => {},
	}

	Ok(())
}

/// Calls the policy server's `/sign` endpoint. The classification of the
/// response (`Signed` / `Refused` / `RateLimited` / `FailOpen`) lets each
/// caller choose its own reaction.
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
	let outgoing = into_outgoing_federation(pdu_json.clone(), room_version);
	let Ok(raw) = to_raw_value(&outgoing) else {
		warn!(via = %policy.via, "failed to serialize PDU for policy /sign; failing open");
		return FetchOutcome::FailOpen;
	};

	let timeout = Duration::from_secs(
		self.services
			.server
			.config
			.policy_server_request_timeout,
	);

	let response = match tokio::time::timeout(
		timeout,
		self.services
			.federation
			.execute(&policy.via, sign_event::Request::new(raw)),
	)
	.await
	{
		| Ok(Ok(response)) => response,
		| Ok(Err(error)) if error.kind() == ErrorKind::Forbidden => return FetchOutcome::Refused,
		| Ok(Err(error)) => {
			if let Some(until_secs) = parse_rate_limit(&error) {
				warn!(via = %policy.via, until_secs, "policy server /sign rate-limited");
				return FetchOutcome::RateLimited { until_secs };
			}
			warn!(via = %policy.via, %error, "policy server /sign failed; failing open");
			return FetchOutcome::FailOpen;
		},
		| Err(elapsed) => {
			warn!(via = %policy.via, %elapsed, "policy server /sign timed out; failing open");
			return FetchOutcome::FailOpen;
		},
	};

	// MSC4284 unstable: a 200 OK with no signature for `via` is also refusal.
	response
		.ed25519_signature(&policy.via)
		.map(ToOwned::to_owned)
		.map_or(FetchOutcome::Refused, FetchOutcome::Signed)
}

fn parse_rate_limit(error: &tuwunel_core::Error) -> Option<u64> {
	let ErrorKind::LimitExceeded(data) = error.kind() else {
		return None;
	};

	let until = match data.retry_after.as_ref()? {
		| RetryAfter::Delay(d) => SystemTime::now().checked_add(*d)?,
		| RetryAfter::DateTime(t) => *t,
	};

	until
		.duration_since(UNIX_EPOCH)
		.ok()
		.map(|d| d.as_secs())
}

/// MSC4284: verify the inbound PDU's policy server signature; if missing, ask
/// the policy server to sign and fold the result in. Mirrors
/// `check_inbound_policy_signature` but upgrades `Missing` to `Pass` (fetched
/// and verified, or fail-open on network error/timeout) or `Invalid`
/// (refused).
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
		| PolicyCheck::Missing =>
			self.fetch_inbound_policy_signature(pdu_json, pdu)
				.await,
		| other => other,
	}
}

/// MSC4284: when an inbound PDU has no policy server signature, ask the
/// policy server to sign on the originator's behalf; fold the returned
/// signature into `pdu_json` so it persists with the event and federates
/// onward. Cached refusals short-circuit to `Invalid`; cached backoffs (or
/// fresh 429s) fail open as `Pass` until the deadline. `Forbidden` from the
/// policy server maps to `Invalid`. Network errors and timeouts fail open
/// with a warn log, mapped to `Pass` since the next server in the room is
/// likely to retry.
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
	match self.cached_policy_state(event_id).await {
		| Some(PolicySigState::Refused) => return PolicyCheck::Invalid,
		| Some(PolicySigState::BackoffUntil { until_secs }) if until_secs > now_secs() => {
			debug!(
				until_secs,
				via = %policy.via,
				"policy server in backoff; failing open"
			);

			return PolicyCheck::Pass;
		},
		| _ => {},
	}

	match self
		.fetch_policy_signature(&policy, pdu_json, &room_version)
		.await
	{
		| FetchOutcome::Signed(signature) => {
			debug!(
				via = %policy.via,
				event_id = %event_id,
				"folded inbound policy server signature"
			);

			insert_policy_signature(pdu_json, &policy.via, &signature);
			PolicyCheck::Pass
		},
		| FetchOutcome::Refused => {
			debug!(
				via = %policy.via,
				event_id = %event_id,
				"policy server refused to sign inbound PDU; soft-failing"
			);

			self.cache_policy_refused(event_id);
			PolicyCheck::Invalid
		},
		| FetchOutcome::RateLimited { until_secs } => {
			self.cache_policy_backoff(event_id, until_secs);
			PolicyCheck::Pass
		},
		| FetchOutcome::FailOpen => PolicyCheck::Pass,
	}
}

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
	if !self.services.server.config.enable_policy_servers {
		return PolicyCheck::NotApplicable;
	}

	if is_policy_state_event(pdu) {
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

	let Ok(rules) = room_version::rules(&room_version) else {
		return PolicyCheck::NotApplicable;
	};

	// `lookup_policy_server` already verified the ed25519 entry is present.
	let Some(public_key) = policy
		.public_keys
		.get(&SigningKeyAlgorithm::Ed25519)
	else {
		return PolicyCheck::NotApplicable;
	};

	let Some(signature_b64) = extract_policy_signature(pdu_json, &policy.via) else {
		return PolicyCheck::Missing;
	};

	let Ok(signature) = Base64::<ruma::serde::base64::Standard>::parse(signature_b64) else {
		return PolicyCheck::Invalid;
	};

	let Ok(redacted) = ruma::canonical_json::redact(pdu_json.clone(), &rules.redaction, None)
	else {
		return PolicyCheck::Invalid;
	};

	let Ok(canonical) = to_canonical_json_string_for_signing(&redacted) else {
		return PolicyCheck::Invalid;
	};

	verify_canonical_json_bytes(
		&SigningKeyAlgorithm::Ed25519,
		public_key.as_bytes(),
		signature.as_bytes(),
		canonical.as_bytes(),
	)
	.map(|()| PolicyCheck::Pass)
	.unwrap_or_else(|error| {
		debug!(via = %policy.via, %error, "policy server signature failed verification");
		PolicyCheck::Invalid
	})
}

fn is_policy_state_event<E: Event>(pdu: &E) -> bool {
	if pdu.state_key() != Some("") {
		return false;
	}

	let kind = pdu.kind().to_cow_str();

	kind == "m.room.policy" || kind == UNSTABLE_POLICY_TYPE
}

fn extract_policy_signature<'a>(
	pdu_json: &'a CanonicalJsonObject,
	via: &ServerName,
) -> Option<&'a str> {
	let CanonicalJsonValue::Object(server_map) = pdu_json.get("signatures")? else {
		return None;
	};

	let CanonicalJsonValue::Object(key_map) = server_map.get(via.as_str())? else {
		return None;
	};

	let CanonicalJsonValue::String(signature) =
		key_map.get(POLICY_SERVER_ED25519_SIGNING_KEY_ID)?
	else {
		return None;
	};

	Some(signature.as_str())
}

fn insert_policy_signature(
	pdu_json: &mut CanonicalJsonObject,
	via: &ServerName,
	signature: &str,
) {
	let signatures = pdu_json
		.entry("signatures".into())
		.or_insert_with(|| CanonicalJsonValue::Object(BTreeMap::new()));

	let CanonicalJsonValue::Object(server_map) = signatures else {
		return;
	};

	let entry = server_map
		.entry(via.as_str().into())
		.or_insert_with(|| CanonicalJsonValue::Object(BTreeMap::new()));

	if let CanonicalJsonValue::Object(key_map) = entry {
		key_map.insert(
			POLICY_SERVER_ED25519_SIGNING_KEY_ID.into(),
			CanonicalJsonValue::String(signature.to_owned()),
		);
	}
}
