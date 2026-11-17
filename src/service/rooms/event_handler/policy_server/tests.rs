use http::Response;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, Int, OwnedEventId, OwnedServerName,
	RoomVersionId, ServerName, UserId,
	api::{
		EndpointError, OutgoingResponseExt, client::uiaa::UiaaResponse, error::Error as RumaError,
	},
	event_id,
	events::room::{member::MembershipState, policy::RoomPolicyEventContent},
	room_id,
	serde::Base64,
	server_name, user_id,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, from_value, json, to_value, to_vec};
use tuwunel_core::{
	Err, Error, Result,
	config::{Figment, rooms::PolicyServer},
	err,
	itertools::Itertools,
	utils::time::now_secs,
};
use tuwunel_database::{Cbor, deserialize_from_slice, serialize_to_vec};
use tuwunel_matrix::pdu::PduEvent;

use super::{
	FetchOutcome, NoVerdict, POLICY_REFUSAL_TTL, PolicyCheck, PolicySigState, Removal,
	check_policy_signature, classify_fetch_error, current_policy_state, inbound_check,
	insert_policy_signature, is_kick, outbound_signature, relayed, removal, retry_until,
	settle_signature, signature_outcome, standard_body, standardized, takes_part,
};
use crate::{
	federation::Classification,
	test_utils::{fixture, pdu_json, with_field, with_state_key},
};

#[derive(Deserialize)]
struct SignatureFixtures {
	via: OwnedServerName,
	public_key: Base64,
	negative_event_id: OwnedEventId,
	vectors: [SignatureVector; 2],
}

#[derive(Deserialize)]
struct SignatureVector {
	room_version: RoomVersionId,
	signature: String,
	pdu: CanonicalJsonObject,
}

#[derive(Serialize)]
enum OldPolicySigState {
	Refused,
	BackoffUntil {
		until_secs: u64,
	},
}

#[test]
fn verifies_policy_server_signature_vectors() {
	let mut fixtures = signature_fixtures();

	for vector in &mut fixtures.vectors {
		insert_policy_signature(&mut vector.pdu, &fixtures.via, vector.signature.clone());

		// Our stored copy carries an `event_id` the signed federation form lacks.
		let stored = stored_copy(vector.pdu.clone(), &fixtures.negative_event_id);

		for pdu in [&vector.pdu, &stored] {
			let check = check_policy_signature(
				pdu,
				&vector.room_version,
				&fixtures.via,
				&fixtures.public_key,
			);

			assert!(
				matches!(check, PolicyCheck::Pass),
				"room version {} policy signature should verify",
				vector.room_version
			);
		}
	}

	let negative = &mut fixtures.vectors[1];

	negative
		.pdu
		.insert("depth".into(), CanonicalJsonValue::Integer(Int::from(6_u8)));

	assert!(matches!(
		check_policy_signature(
			&negative.pdu,
			&negative.room_version,
			&fixtures.via,
			&fixtures.public_key,
		),
		PolicyCheck::Invalid
	));
}

fn stored_copy(mut pdu: CanonicalJsonObject, event_id: &EventId) -> CanonicalJsonObject {
	pdu.insert("event_id".into(), CanonicalJsonValue::String(event_id.as_str().to_owned()));
	pdu
}

#[test]
fn a_verified_signature_is_folded_in() {
	let fixtures = signature_fixtures();
	let policy = fixture_policy(&fixtures);

	for vector in &fixtures.vectors {
		let stored = stored_copy(vector.pdu.clone(), &fixtures.negative_event_id);
		let outcome =
			signature_outcome(&stored, &vector.room_version, &policy, Some(&vector.signature));

		assert!(
			matches!(&outcome, FetchOutcome::Signed(signature) if *signature == vector.signature)
		);

		assert!(matches!(inbound_check(&outcome, false), PolicyCheck::Pass));
		assert_eq!(
			outbound_signature(outcome, false, &fixtures.via).expect("a signed event is sent"),
			Some(vector.signature.clone())
		);
	}
}

#[test]
fn a_refusal_rejects_whatever_the_leniency() {
	let fixtures = signature_fixtures();
	let vector = &fixtures.vectors[0];
	let spam = json!({"errcode": "M_FORBIDDEN", "error": "The message appears to contain possible spam"});

	let answered = classify_fetch_error(federation_error(400, &spam));
	let unsigned =
		signature_outcome(&vector.pdu, &vector.room_version, &fixture_policy(&fixtures), None);

	assert!(matches!(answered, FetchOutcome::Refused(Some(_))));
	assert!(matches!(unsigned, FetchOutcome::Refused(None)));

	for outcome in [&answered, &unsigned] {
		assert!(matches!(inbound_check(outcome, true), PolicyCheck::Invalid));
		assert!(inbound_check(outcome, true).soft_fails());
	}

	let generic = json!({
		"errcode": "M_FORBIDDEN",
		"error": "M_FORBIDDEN: Event was rejected by the room's policy server.",
	});

	assert_eq!(rejection(answered, true), (400, spam));
	assert_eq!(rejection(unsigned, true), (403, generic));
}

#[test]
fn a_signature_failing_verification_is_withheld_and_rejects_unless_lenient() {
	let fixtures = signature_fixtures();
	let [first, second] = &fixtures.vectors;
	let policy = fixture_policy(&fixtures);
	let before = first.pdu.clone();

	let unverified =
		|| signature_outcome(&first.pdu, &first.room_version, &policy, Some(&second.signature));

	assert!(matches!(unverified(), FetchOutcome::NoVerdict(NoVerdict::Unverified)));
	assert_eq!(first.pdu, before, "a failing signature never reaches the event");
	assert_no_verdict_dispositions(unverified, 503, &unavailable(&fixtures.via));
}

#[test]
fn an_error_answer_rejects_unless_lenient() {
	let via = server_name!("policy.example.org");

	for (status, body) in [
		(400, json!({"errcode": "M_BAD_JSON", "error": "Missing keys"})),
		(400, json!({"errcode": "M_NOT_JSON", "error": "Not JSON"})),
		(403, json!({"errcode": "M_FORBIDDEN", "error": "Your server is not allowed"})),
		(404, json!({"errcode": "M_NOT_FOUND", "error": "Unknown room"})),
		(404, json!({"errcode": "M_UNRECOGNIZED", "error": "Not a policy server"})),
		(500, json!({"errcode": "M_UNKNOWN", "error": "Internal error"})),
		(502, json!({"errcode": "M_UNKNOWN", "error": "Bad gateway"})),
	] {
		let outcome = || classify_fetch_error(federation_error(status, &body));

		assert!(matches!(outcome(), FetchOutcome::NoVerdict(NoVerdict::Failed(_))), "{status}");
		assert_no_verdict_dispositions(outcome, status, &body);
	}

	let unauthorized = json!({"errcode": "M_UNAUTHORIZED", "error": "Bad X-Matrix signature"});
	let outcome = || classify_fetch_error(federation_error(401, &unauthorized));

	assert_no_verdict_dispositions(outcome, 403, &unauthorized);

	for transport in [
		err!(BadServerResponse("connection refused")),
		err!("Won't send federation request to ourselves"),
		federation_error_not_json(502),
	] {
		let outcome = classify_fetch_error(transport);

		assert!(matches!(&outcome, FetchOutcome::NoVerdict(NoVerdict::Unavailable)));
		assert_eq!(rejection(outcome, false), (503, unavailable(via)));
	}
}

#[test]
fn a_rate_limit_rejects_unless_lenient_and_names_its_backoff() {
	let limited =
		json!({"errcode": "M_LIMIT_EXCEEDED", "error": "Slow down", "retry_after_ms": 120_000});

	let outcome = || classify_fetch_error(federation_error(429, &limited));

	assert!(matches!(outcome(), FetchOutcome::NoVerdict(NoVerdict::Failed(_))));
	assert_no_verdict_dispositions(outcome, 429, &limited);

	let without = json!({"errcode": "M_LIMIT_EXCEEDED", "error": "Slow down"});
	let other = json!({"errcode": "M_UNKNOWN", "error": "Internal error"});

	assert_eq!(retry_until(&remote(429, &limited), 1_000), Some(1_120));
	assert_eq!(retry_until(&remote(429, &without), 1_000), None);
	assert_eq!(retry_until(&remote(500, &other), 1_000), None);
	let backed_off = || FetchOutcome::NoVerdict(NoVerdict::Unavailable);

	assert_no_verdict_dispositions(
		backed_off,
		503,
		&unavailable(server_name!("policy.example.org")),
	);
}

#[test]
fn a_local_refusal_rejects_unless_lenient() {
	let refused = || classify_fetch_error(err!(Request(Forbidden("Federation is disabled."))));
	let configuration = json!({
		"errcode": "M_FORBIDDEN",
		"error": "M_FORBIDDEN: This server's configuration does not permit contacting the room's policy server.",
	});

	assert!(matches!(refused(), FetchOutcome::NoVerdict(NoVerdict::NotPermitted)));
	assert_no_verdict_dispositions(refused, 403, &configuration);
}

#[tokio::test]
async fn a_backed_off_policy_server_is_not_asked() -> Result {
	let config =
		Figment::new().merge(("federation.forbidden_remote_server_names", [r"\.example$"]));

	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let backed_off = server_name!("backed-off.example");
	let forbidden = server_name!("forbidden.example");
	let fetch = async |via: &ServerName| {
		let policy = RoomPolicyEventContent::new(via.to_owned(), signature_fixtures().public_key);

		services
			.event_handler
			.fetch_policy_signature(&policy, &CanonicalJsonObject::new(), &RoomVersionId::V12)
			.await
	};

	services
		.federation
		.record_failure(backed_off, Classification::Transient);

	// Both names are forbidden, so asking the backed-off one would be refused.
	let skipped = fetch(backed_off).await;
	let refused = fetch(forbidden).await;

	assert!(matches!(skipped, FetchOutcome::NoVerdict(NoVerdict::Unavailable)));
	assert!(matches!(refused, FetchOutcome::NoVerdict(NoVerdict::NotPermitted)));

	Ok(())
}

#[test]
fn leaves_kicks_and_bans_take_access_away() {
	let alice = user_id!("@alice:example.org");
	let bob = user_id!("@bob:example.org");
	let message = message_event(alice);

	assert_eq!(removal(&member_event(alice, alice, "leave")), Some(Removal::Leave));
	assert_eq!(removal(&member_event(alice, bob, "ban")), Some(Removal::Ban));
	assert_eq!(removal(&member_event(alice, bob, "leave")), Some(Removal::KickOrUnban(bob)));
	assert_eq!(removal(&member_event(alice, alice, "join")), None);
	assert_eq!(removal(&member_event(alice, bob, "invite")), None);
	assert_eq!(removal(&message), None);
}

#[tokio::test]
async fn only_access_removals_go_out_without_a_verdict() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let event_handler = &fixture.services.event_handler;
	let alice = user_id!("@alice:localhost");
	let bob = user_id!("@bob:localhost");

	for (sender, target, membership, tolerated) in [
		(alice, alice, "leave", true),
		(alice, bob, "ban", true),
		(alice, bob, "leave", false),
		(alice, alice, "join", false),
	] {
		let event = member_event(sender, target, membership);
		let actual = event_handler
			.tolerates_missing_verdict(&event)
			.await;

		assert_eq!(actual, tolerated, "{membership} of {target} by {sender}");
	}

	let message = message_event(alice);
	let tolerated = event_handler
		.tolerates_missing_verdict(&message)
		.await;

	assert!(!tolerated, "a message waits for a verdict");

	Ok(())
}

#[test]
fn only_a_member_with_access_leaving_is_a_kick() {
	for membership in [MembershipState::Join, MembershipState::Invite, MembershipState::Knock] {
		assert!(is_kick(Ok(membership.clone())), "{membership}");
	}

	assert!(!is_kick(Ok(MembershipState::Ban)), "lifting a ban grants access");
	assert!(!is_kick(Ok(MembershipState::Leave)));
	assert!(!is_kick(Err!(Database("unreadable membership"))));
}

#[test]
fn relays_policy_server_errors_verbatim() {
	for (status, body) in [
		(
			400,
			json!({"errcode": "M_FORBIDDEN", "error": "The message appears to contain possible spam"}),
		),
		(404, json!({"errcode": "M_NOT_FOUND", "error": "Unknown room"})),
		(
			429,
			json!({"errcode": "M_LIMIT_EXCEEDED", "error": "Slow down", "retry_after_ms": 2_000}),
		),
		(500, json!({"errcode": "M_UNKNOWN", "error": "Internal error"})),
	] {
		assert_eq!(rendered(relayed(remote(status, &body))), (status, body.clone()));
	}

	let unauthorized = json!({"errcode": "M_UNAUTHORIZED", "error": "Bad X-Matrix signature"});

	assert_eq!(rendered(relayed(remote(401, &unauthorized))), (403, unauthorized.clone()));

	// Only the message survives, whatever the status; no field of the remote body
	// reaches the client.
	let sessions = [
		"M_UNKNOWN_TOKEN",
		"M_MISSING_TOKEN",
		"M_USER_DEACTIVATED",
		"M_USER_LOCKED",
		"M_USER_SUSPENDED",
		"M_CONSENT_NOT_GIVEN",
		"M_RESOURCE_LIMIT_EXCEEDED",
	];

	[400, 401, 403]
		.into_iter()
		.cartesian_product(sessions)
		.for_each(|(status, errcode)| {
			let session = json!({
				"errcode": errcode,
				"error": "Not your session",
				"soft_logout": true,
				"consent_uri": "https://policy.example.org/consent",
				"admin_contact": "mailto:admin@policy.example.org",
			});

			let neutral = json!({"errcode": "M_FORBIDDEN", "error": "Not your session"});

			assert_eq!(
				rendered(relayed(remote(status, &session))),
				(403, neutral),
				"{status} {errcode}"
			);
		});
}

#[test]
fn a_message_less_error_is_still_a_matrix_error() {
	let refusal = json!({"errcode": "M_FORBIDDEN"});
	let limited = json!({"errcode": "M_LIMIT_EXCEEDED", "retry_after_ms": 120_000});
	let unlabelled = json!({"error": "No errcode"});

	let refused = classify_fetch_error(standardized(federation_error(400, &refusal)));
	let unknown = classify_fetch_error(standardized(federation_error(400, &unlabelled)));

	assert!(matches!(&refused, FetchOutcome::Refused(Some(_))));
	assert!(matches!(unknown, FetchOutcome::NoVerdict(NoVerdict::Unavailable)));
	assert_eq!(retry_until(&standard_body(remote(429, &limited)), 1_000), Some(1_120));
	assert_eq!(rejection(refused, true), (400, json!({"errcode": "M_FORBIDDEN", "error": ""})));
}

#[tokio::test]
async fn a_message_less_answer_is_classified_and_backs_off() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let via = server_name!("policy.example.org");
	let limited = json!({"errcode": "M_LIMIT_EXCEEDED", "retry_after_ms": 3_600_000});
	let refusal = json!({"errcode": "M_FORBIDDEN"});
	let now = now_secs();

	let throttled = services
		.event_handler
		.failed_fetch(via, federation_error(429, &limited));

	let refused = services
		.event_handler
		.failed_fetch(via, federation_error(400, &refusal));

	let backoff = services
		.federation
		.peer_backoff(via)
		.await
		.expect("the retry time is recorded");

	assert!(matches!(throttled, FetchOutcome::NoVerdict(NoVerdict::Failed(_))));
	assert!(matches!(refused, FetchOutcome::Refused(Some(_))));
	assert!(backoff.anchor_secs + backoff.delay_secs >= now + 3_600, "{backoff:?}");

	Ok(())
}

#[test]
fn settling_strips_a_policy_signature_that_is_not_replaced() {
	let stale = json!({"ed25519:policy_server": "stale"});
	let shared = json!({"ed25519:policy_server": "stale", "ed25519:origin": "own"});

	let unsigned = json!({"ed25519:origin": "own"});

	let replaced = settled(&stale, FetchOutcome::Signed("fresh".to_owned()));
	let kept = settled(&shared, FetchOutcome::NoVerdict(NoVerdict::Unavailable));
	let removed = settled(&stale, FetchOutcome::Refused(None));
	let untouched = settled(&unsigned, FetchOutcome::NoVerdict(NoVerdict::Unavailable));

	let only = |keys: Value| json!({ "policy.example.org": keys });

	assert_eq!(replaced, (only(json!({"ed25519:policy_server": "fresh"})), true));
	assert_eq!(kept, (only(json!({"ed25519:origin": "own"})), true));
	assert_eq!(removed, (json!({}), true));
	assert_eq!(untouched, (only(unsigned), false));
}

fn settled(keys: &Value, outcome: FetchOutcome) -> (Value, bool) {
	let via = server_name!("policy.example.org");
	let pdu =
		from_value(json!({ "signatures": { via.as_str(): keys } })).expect("a canonical event");

	let (pdu, changed) = settle(pdu, via, outcome);
	let signatures = to_value(&pdu).expect("the event serializes")["signatures"].clone();

	(signatures, changed)
}

fn settle(
	mut pdu: CanonicalJsonObject,
	via: &ServerName,
	outcome: FetchOutcome,
) -> (CanonicalJsonObject, bool) {
	let changed = settle_signature(&mut pdu, via, outcome);

	(pdu, changed)
}

#[tokio::test]
async fn a_settled_signature_is_stored_where_it_is_served() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let via = server_name!("policy.example.org");
	let event_id = event_id!("$stored:localhost");
	let signatures = json!({
		"policy.example.org": {"ed25519:policy_server": "invalid"},
		"origin.example": {"ed25519:origin": "own"},
	});

	let event = message_json(event_id, user_id!("@alice:origin.example"));
	let mut pdu: CanonicalJsonObject =
		from_value(with_field(event, "signatures", signatures)).expect("a canonical event");

	services.timeline.add_pdu_outlier(event_id, &pdu);
	services.event_handler.settle_and_store(
		event_id,
		&mut pdu,
		via,
		FetchOutcome::NoVerdict(NoVerdict::Unavailable),
	);

	let served = services.timeline.get_pdu_json(event_id).await?;
	let kept = json!({"origin.example": {"ed25519:origin": "own"}});

	assert_eq!(to_value(&served)?["signatures"], kept);

	Ok(())
}

fn message_json(event_id: &EventId, sender: &UserId) -> Value {
	let content = json!({ "msgtype": "m.text", "body": "hello" });

	pdu_json(event_id, room_id!("!room:localhost"), sender, "m.room.message", &content)
}

#[test]
fn takes_part_only_with_both_switches_on() {
	let default = PolicyServer::default();

	assert!(takes_part(&default));
	assert!(!takes_part(&PolicyServer { enable: false, ..default }));
	assert!(!takes_part(&PolicyServer { advisory_room_versions: false, ..default }));
}

#[test]
fn expires_policy_refusals() {
	let current = current_policy_state(Ok(Cbor(PolicySigState::Refused { until_secs: 100 })), 99);

	assert!(matches!(current, Some(PolicySigState::Refused { until_secs: 100 })));

	assert!(
		current_policy_state(Ok(Cbor(PolicySigState::Refused { until_secs: 100 })), 100,)
			.is_none()
	);
	assert_eq!(POLICY_REFUSAL_TTL.as_secs(), 24 * 60 * 60);
}

#[test]
fn treats_old_policy_states_as_absent() {
	let backoff = OldPolicySigState::BackoffUntil { until_secs: u64::MAX };

	for old in [OldPolicySigState::Refused, backoff] {
		let encoded = serialize_to_vec(Cbor(&old)).expect("old state should serialize");
		let decoded = deserialize_from_slice::<Cbor<PolicySigState>>(&encoded);

		assert!(current_policy_state(decoded, 0).is_none());
	}
}

/// Checks a no-verdict outcome under both leniencies.
///
/// Unlenient, a local event is rejected with `status` and `body`, and an
/// incoming one has no verdict, which soft-fails it when received live;
/// lenient, the first goes out unsigned and the second passes.
fn assert_no_verdict_dispositions<F>(outcome: F, status: u16, body: &Value)
where
	F: Fn() -> FetchOutcome,
{
	let via = server_name!("policy.example.org");

	assert!(matches!(inbound_check(&outcome(), false), PolicyCheck::NoVerdict));
	assert!(inbound_check(&outcome(), false).soft_fails());
	assert!(matches!(inbound_check(&outcome(), true), PolicyCheck::Pass));
	assert!(!inbound_check(&outcome(), true).soft_fails());
	assert_eq!(rejection(outcome(), false), (status, body.clone()));
	assert_eq!(outbound_signature(outcome(), true, via).expect("a lenient send goes out"), None);
}

fn rejection(outcome: FetchOutcome, tolerated: bool) -> (u16, Value) {
	let via = server_name!("policy.example.org");
	let error = outbound_signature(outcome, tolerated, via).expect_err("the send is refused");

	rendered(error)
}

fn rendered(error: Error) -> (u16, Value) {
	let response = UiaaResponse::from(error)
		.try_into_http_response::<Vec<u8>>()
		.expect("the error response serializes");

	let body = serde_json::from_slice(response.body()).expect("the error body is JSON");

	(response.status().as_u16(), body)
}

fn unavailable(via: &ServerName) -> Value {
	json!({
		"errcode": "M_UNKNOWN",
		"error": format!("M_UNKNOWN: The room's policy server {via} is unavailable."),
	})
}

fn remote(status: u16, body: &Value) -> RumaError {
	let body = to_vec(body).expect("the error body serializes");
	let response = Response::builder()
		.status(status)
		.body(body.as_slice())
		.expect("the response builds");

	RumaError::from_http_response(response)
}

fn federation_error(status: u16, body: &Value) -> Error {
	Error::Federation(server_name!("policy.example.org").to_owned(), remote(status, body))
}

fn federation_error_not_json(status: u16) -> Error {
	let response = Response::builder()
		.status(status)
		.body(b"<html>Bad gateway</html>".as_slice())
		.expect("the response builds");

	Error::Federation(
		server_name!("policy.example.org").to_owned(),
		RumaError::from_http_response(response),
	)
}

fn signature_fixtures() -> SignatureFixtures {
	serde_json::from_str(include_str!("fixtures/signature_vectors.json"))
		.expect("signature fixture should deserialize")
}

fn fixture_policy(fixtures: &SignatureFixtures) -> RoomPolicyEventContent {
	RoomPolicyEventContent::new(fixtures.via.clone(), fixtures.public_key.clone())
}

fn member_event(sender: &UserId, target: &UserId, membership: &str) -> PduEvent {
	let content = json!({ "membership": membership });
	let event = pdu_json(
		event_id!("$member:localhost"),
		room_id!("!room:localhost"),
		sender,
		"m.room.member",
		&content,
	);

	from_value(with_state_key(event, target.as_str())).expect("a member event deserializes")
}

fn message_event(sender: &UserId) -> PduEvent {
	let event = message_json(event_id!("$message:localhost"), sender);

	from_value(event).expect("a message event deserializes")
}
