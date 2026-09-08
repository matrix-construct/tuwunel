use http::StatusCode;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, OwnedServerName, RoomVersionId,
	api::error::{ErrorKind, LimitExceededErrorData},
	events::room::policy::RoomPolicyEventContent,
	serde::Base64,
};
use serde::{Deserialize, Serialize};
use tuwunel_database::{Cbor, deserialize_from_slice, serialize_to_vec};

use super::{
	FetchOutcome, POLICY_REFUSAL_TTL, PolicyCheck, PolicySigState, Reaction,
	check_policy_signature, classify_fetch_error, current_policy_state, insert_policy_signature,
	react_to_fetch, resolve_policy_target,
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

#[derive(Clone, Copy)]
enum ExpectedFetchOutcome {
	FailOpen,
	Refused,
	RateLimited(u64),
}

#[derive(Serialize)]
enum OldPolicySigState {
	Refused,
}

#[test]
fn verifies_policy_server_signature_vectors() {
	let mut fixtures: SignatureFixtures =
		serde_json::from_str(include_str!("fixtures/signature_vectors.json"))
			.expect("signature fixture should deserialize");

	for vector in &mut fixtures.vectors {
		insert_policy_signature(&mut vector.pdu, &fixtures.via, &vector.signature);

		assert!(
			matches!(
				check_policy_signature(
					&vector.pdu,
					&vector.room_version,
					&fixtures.via,
					&fixtures.public_key,
				),
				PolicyCheck::Pass
			),
			"room version {} policy signature should verify",
			vector.room_version
		);
	}

	let negative = &mut fixtures.vectors[1];

	negative.pdu.insert(
		"event_id".into(),
		CanonicalJsonValue::String(fixtures.negative_event_id.to_string()),
	);

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

#[test]
fn classifies_policy_server_errors() {
	let cases = [
		(
			"400 forbidden",
			StatusCode::BAD_REQUEST,
			ErrorKind::Forbidden,
			None,
			ExpectedFetchOutcome::Refused,
		),
		(
			"400 bad json",
			StatusCode::BAD_REQUEST,
			ErrorKind::BadJson,
			None,
			ExpectedFetchOutcome::FailOpen,
		),
		(
			"400 not json",
			StatusCode::BAD_REQUEST,
			ErrorKind::NotJson,
			None,
			ExpectedFetchOutcome::FailOpen,
		),
		(
			"401",
			StatusCode::UNAUTHORIZED,
			ErrorKind::Unknown,
			None,
			ExpectedFetchOutcome::FailOpen,
		),
		(
			"403 forbidden",
			StatusCode::FORBIDDEN,
			ErrorKind::Forbidden,
			None,
			ExpectedFetchOutcome::FailOpen,
		),
		(
			"404 not found",
			StatusCode::NOT_FOUND,
			ErrorKind::NotFound,
			None,
			ExpectedFetchOutcome::FailOpen,
		),
		(
			"404 unrecognized",
			StatusCode::NOT_FOUND,
			ErrorKind::Unrecognized,
			None,
			ExpectedFetchOutcome::FailOpen,
		),
		(
			"429 rate limited",
			StatusCode::TOO_MANY_REQUESTS,
			ErrorKind::LimitExceeded(LimitExceededErrorData::new()),
			Some(100),
			ExpectedFetchOutcome::RateLimited(100),
		),
		(
			"unexpected status rate limited",
			StatusCode::IM_A_TEAPOT,
			ErrorKind::LimitExceeded(LimitExceededErrorData::new()),
			Some(200),
			ExpectedFetchOutcome::RateLimited(200),
		),
		(
			"server error",
			StatusCode::INTERNAL_SERVER_ERROR,
			ErrorKind::Unknown,
			None,
			ExpectedFetchOutcome::FailOpen,
		),
		(
			"other response",
			StatusCode::IM_A_TEAPOT,
			ErrorKind::Unknown,
			None,
			ExpectedFetchOutcome::FailOpen,
		),
	];

	for (name, status, errcode, rate_limit_until, expected) in cases {
		let outcome = classify_fetch_error(status, &errcode, rate_limit_until);
		let matches = match (expected, outcome) {
			| (ExpectedFetchOutcome::FailOpen, FetchOutcome::FailOpen) => true,
			| (
				ExpectedFetchOutcome::Refused,
				FetchOutcome::Refused {
					status: actual_status,
					errcode: Some(ErrorKind::Forbidden),
				},
			) => actual_status == status,
			| (
				ExpectedFetchOutcome::RateLimited(expected_until),
				FetchOutcome::RateLimited { until_secs },
			) => until_secs == expected_until,
			| _ => false,
		};

		assert!(matches, "{name}");
	}
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
fn treats_old_refusal_encoding_as_absent() {
	let encoded =
		serialize_to_vec(Cbor(&OldPolicySigState::Refused)).expect("old state should serialize");

	let decoded = deserialize_from_slice::<Cbor<PolicySigState>>(&encoded);

	assert!(current_policy_state(decoded, 0).is_none());
}

/// Fail-open versus fail-closed is one function, so it is one table: refusal
/// always rejects, a signature always proceeds, and only the transport-shaped
/// outcomes follow the switch.
#[test]
fn reacts_to_fetch_outcomes_per_fail_closed_switch() {
	let cases = [
		("signed, open", FetchOutcome::Signed("sig".to_owned()), false, Reaction::Proceed),
		(
			"signed, closed",
			FetchOutcome::Signed("sig".to_owned()),
			true,
			Reaction::Proceed,
		),
		(
			"refused, open",
			FetchOutcome::Refused {
				status: StatusCode::BAD_REQUEST,
				errcode: Some(ErrorKind::Forbidden),
			},
			false,
			Reaction::Reject,
		),
		(
			"refused, closed",
			FetchOutcome::Refused {
				status: StatusCode::BAD_REQUEST,
				errcode: Some(ErrorKind::Forbidden),
			},
			true,
			Reaction::Reject,
		),
		("transport, open", FetchOutcome::FailOpen, false, Reaction::Proceed),
		("transport, closed", FetchOutcome::FailOpen, true, Reaction::Reject),
		(
			"rate-limited, open",
			FetchOutcome::RateLimited { until_secs: 1 },
			false,
			Reaction::Proceed,
		),
		(
			"rate-limited, closed",
			FetchOutcome::RateLimited { until_secs: 1 },
			true,
			Reaction::Reject,
		),
	];

	for (name, outcome, fail_closed, expected) in cases {
		assert_eq!(react_to_fetch(&outcome, fail_closed), expected, "{name}");
	}
}

/// The mandatory server takes precedence over the room's own, and a room's
/// own is used only when no mandatory server is configured.
#[test]
fn mandatory_policy_server_wins_over_the_room_state() {
	let key = Base64::parse("A6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg").expect("valid base64");
	let room_key =
		Base64::parse("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").expect("valid base64");
	let mandatory_name: OwnedServerName = "policy.internal".try_into().expect("valid name");
	let room_name: OwnedServerName = "policy.example.org"
		.try_into()
		.expect("valid name");
	let mandatory =
		Some((mandatory_name.clone(), key, "https://policy.internal:8448".to_owned()));
	let room = Some(RoomPolicyEventContent::new(room_name.clone(), room_key));

	let target =
		resolve_policy_target(mandatory.clone(), room.clone()).expect("mandatory target");
	assert_eq!(target.content.via, mandatory_name);
	assert_eq!(target.direct_url.as_deref(), Some("https://policy.internal:8448"));

	let target = resolve_policy_target(mandatory, None).expect("mandatory target without room");
	assert_eq!(target.content.via, mandatory_name);

	let target = resolve_policy_target(None, room).expect("room target");
	assert_eq!(target.content.via, room_name);
	assert!(target.direct_url.is_none(), "a room's own server is resolved by federation");

	assert!(resolve_policy_target(None, None).is_none());
}
