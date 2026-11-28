use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use serde_json::{Value as JsonValue, json};

use super::{Expected, check_claims};

const ISSUER: &str = "https://idp.example";
const CLIENT: &str = "tuwunel";
const NONCE: &str = "n-0S6_WzA2Mj";
const NOW: u64 = 1_700_000_000;

#[test]
fn a_conforming_token_is_accepted() {
	assert_eq!(check_claims(&token(&claims()), &expected()), Ok(()));
}

#[test]
fn each_refusal_names_its_claim() {
	let cases = [
		("iss", with("iss", json!("https://elsewhere.example"))),
		("iss", without("iss")),
		("sub", with("sub", json!(null))),
		("sub", without("sub")),
		("aud", with("aud", json!("another-client"))),
		("aud", with("aud", json!(["another-client", "a-third"]))),
		("aud", with("aud", json!([]))),
		("aud", set(with("aud", json!("another-client")), "azp", json!(CLIENT))),
		("aud", without("aud")),
		("azp", with("azp", json!("another-client"))),
		("azp", with("aud", json!(["another-client", CLIENT]))),
		("exp", with("exp", json!(NOW - 120))),
		("exp", with("exp", json!(-1))),
		("exp", without("exp")),
		("iat", without("iat")),
		("nonce", with("nonce", json!("another-nonce"))),
		("nonce", without("nonce")),
	];

	for (refused, claims) in cases {
		assert_eq!(check_claims(&token(&claims), &expected()), Err(refused), "{claims}");
	}
}

#[test]
fn spellings_a_conforming_provider_uses_are_accepted() {
	let cases = [
		with("iss", json!("https://idp.example/")),
		with("aud", json!([CLIENT])),
		with("azp", json!(CLIENT)),
		set(with("aud", json!(["another-client", CLIENT])), "azp", json!(CLIENT)),
		with("exp", json!(NOW - 119)),
		with("exp", json!(1_700_000_300.5)),
		with("iat", json!(1_699_999_999.5)),
	];

	for claims in cases {
		assert_eq!(check_claims(&token(&claims), &expected()), Ok(()), "{claims}");
	}
}

#[test]
fn a_provider_without_an_issuer_accepts_no_token() {
	let expected = Expected { issuer: None, ..expected() };

	assert_eq!(check_claims(&token(&claims()), &expected), Err("iss"));
}

#[test]
fn a_nonce_is_checked_only_when_one_was_sent() {
	let expected = Expected { nonce: None, ..expected() };

	assert_eq!(check_claims(&token(&claims()), &expected), Ok(()));
	assert_eq!(check_claims(&token(&without("nonce")), &expected), Ok(()));
}

#[test]
fn only_a_three_segment_token_is_read() {
	let claims = segment(&claims());
	let header = segment(&json!({"alg": "RS256"}));
	let cases = [
		"id.code".to_owned(),
		format!("{header}.not-base64!.signature"),
		format!("{header}.{}.signature", b64.encode("not json")),
		format!("{header}.key.iv.{claims}.tag"),
		format!("{header}.{claims}"),
	];

	for token in cases {
		assert_eq!(check_claims(&token, &expected()), Err("format"), "{token}");
	}
}

fn token(claims: &JsonValue) -> String {
	let header = segment(&json!({"alg": "RS256"}));
	let claims = segment(claims);

	format!("{header}.{claims}.signature")
}

fn segment(value: &JsonValue) -> String { b64.encode(value.to_string()) }

fn claims() -> JsonValue {
	json!({
		"iss": ISSUER,
		"sub": "alice",
		"aud": CLIENT,
		"exp": NOW + 300,
		"iat": NOW,
		"nonce": NONCE,
	})
}

fn expected() -> Expected<'static> {
	Expected {
		issuer: Some(ISSUER),
		client_id: CLIENT,
		nonce: Some(NONCE),
		now: Duration::from_secs(NOW),
	}
}

fn with(claim: &str, value: JsonValue) -> JsonValue { set(claims(), claim, value) }

fn set(mut claims: JsonValue, claim: &str, value: JsonValue) -> JsonValue {
	claims[claim] = value;
	claims
}

fn without(claim: &str) -> JsonValue {
	let mut claims = claims();

	claims
		.as_object_mut()
		.expect("claims object")
		.remove(claim);

	claims
}
