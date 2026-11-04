use std::{
	net::{IpAddr, Ipv4Addr},
	time::{Duration, Instant},
};

use futures::TryStreamExt;
use ruma::user_id;
use serde_json::{Value as JsonValue, from_value, json};
use tuwunel_core::{Result, config::Figment, result::NotFound};
use tuwunel_database::{Cbor, deserialize_from_slice, serialize_to_vec};

use super::{Deletions, Ratelimiter, Session, TokenResponse, UserInfo, check_bucket_at};
use crate::test_utils::fixture;

#[test]
fn rate_limiter_evicts_oldest_without_exceeding_cap() {
	let now = Instant::now();
	let oldest = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
	let retained = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
	let incoming = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 3));
	let table = Ratelimiter::new(
		[(oldest, (now, 1.5)), (retained, (now + Duration::from_millis(1), 1.5))].into(),
	);

	check_bucket_at(&table, incoming, 0.0, 2.0, now + Duration::from_millis(2), 2)
		.expect("new client should be accepted");

	let buckets = table
		.lock()
		.expect("rate limiter table lock should not be poisoned");

	assert_eq!(buckets.len(), 2, "rate-limit table must stay capped");
	assert!(!buckets.contains_key(&oldest), "oldest bucket should be evicted");
	assert!(buckets.contains_key(&retained), "newer bucket should be retained");
	assert!(buckets.contains_key(&incoming), "incoming bucket should be inserted");
	drop(buckets);

	check_bucket_at(&table, incoming, 0.0, 2.0, now + Duration::from_millis(3), 2)
		.expect("existing client should be accepted");

	let buckets = table
		.lock()
		.expect("rate limiter table lock should not be poisoned");

	assert_eq!(buckets.len(), 2, "existing key must not grow the table");
	assert!(buckets.contains_key(&retained), "existing hit must not evict another bucket");
}

#[test]
fn session_debug_masks_credentials() {
	let secrets = [
		"access_token",
		"id_token",
		"refresh_token",
		"code_verifier",
		"cookie_nonce",
		"query_nonce",
	];

	let [access_token, id_token, refresh_token, code_verifier, cookie_nonce, query_nonce] =
		secrets.map(sentinel).map(Some);

	let session = Session {
		sess_id: Some("visible-session".to_owned()),
		access_token,
		id_token,
		refresh_token,
		code_verifier,
		cookie_nonce,
		query_nonce,
		..Default::default()
	};

	let dump = format!("{session:#?}");

	assert!(dump.contains("visible-session"), "sess_id should stay visible: {dump}");
	assert_masked(&dump, &secrets);
}

fn sentinel(field: &str) -> String { format!("{field}-sentinel") }

fn assert_masked(dump: &str, fields: &[&str]) {
	for field in fields {
		assert!(!dump.contains(&sentinel(field)), "{field} leaked in Debug: {dump}");
		assert!(
			dump.contains(&format!("{field}: Some(<redacted>)")),
			"{field} should appear as Some(<redacted>): {dump}"
		);
	}
}

#[test]
fn token_response_debug_masks_credentials() {
	let secrets = ["access_token", "refresh_token", "id_token"];
	let [access_token, refresh_token, id_token] = secrets.map(sentinel);
	let response: TokenResponse = from_value(json!({
		"token_type": "visible-type",
		"access_token": access_token,
		"refresh_token": refresh_token,
		"id_token": id_token,
	}))
	.expect("token response");

	let dump = format!("{response:#?}");

	assert!(dump.contains("visible-type"), "token_type should stay visible: {dump}");
	assert_masked(&dump, &secrets);
}

#[tokio::test]
async fn deleting_user_sessions_includes_unresolved_providers() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let oauth = &fixture.services.oauth;
	let user_id = user_id!("@stale:localhost");
	let session = |sess_id: &str, user_info| Session {
		idp_id: Some("removed-idp".to_owned()),
		sess_id: Some(sess_id.to_owned()),
		user_id: Some(user_id.to_owned()),
		user_info,
		..Default::default()
	};

	let linked = UserInfo {
		sub: "stale".to_owned(),
		..Default::default()
	};

	oauth
		.sessions
		.put(&session("linked", Some(linked)))
		.await;

	// Bound while the provider was configured, under a key only it derives.
	fixture.services.db["oauthuniqid_oauthid"].insert("stale-identity", "linked");

	oauth
		.sessions
		.put(&session("pending", None))
		.await;

	let Deletions { complete, unresolved, absent, unreadable } =
		oauth.delete_user_sessions(user_id).await?;

	let listed = oauth
		.sessions
		.get_sess_id_by_user(user_id)
		.try_collect::<Vec<_>>()
		.await;

	let binding = oauth
		.sessions
		.get_sess_id_by_unique_id("stale-identity")
		.await?;

	let bound = oauth
		.sessions
		.get_by_unique_id("stale-identity")
		.await;

	assert_eq!((complete, unresolved, absent, unreadable), (1, 1, 0, 0));
	assert!(oauth.sessions.get("linked").await.is_missing(), "linked session deleted");
	assert!(oauth.sessions.get("pending").await.is_missing(), "pending session deleted");
	assert!(listed.is_missing(), "the user's index row deleted");
	assert_eq!(binding, "linked", "the binding no provider derives remains");
	assert!(bound.is_missing(), "the remaining binding resolves to no session");
	assert!(oauth.sessions.delete("linked").await.is_missing(), "absent session is an error");

	Ok(())
}

#[tokio::test]
async fn deleting_a_session_keeps_it_while_its_user_index_cannot_be_read() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let sessions = &fixture.services.oauth.sessions;
	let user_id = user_id!("@faulted:localhost");
	let session = Session {
		sess_id: Some("faulted".to_owned()),
		user_id: Some(user_id.to_owned()),
		..Default::default()
	};

	sessions.put(&session).await;

	// A lone continuation byte is not UTF-8, so the read fails without reading as absent.
	fixture.services.db["userid_oauthid"].insert(user_id.as_str(), [0x80]);

	let deleted = sessions.delete("faulted").await;
	let kept = sessions.get("faulted").await;

	assert!(deleted.is_err() && !deleted.is_missing(), "the unreadable index is an error");
	assert!(kept.is_ok(), "the session is kept");

	Ok(())
}

#[test]
fn userinfo_reads_email_verified_leniently() {
	let read = |claim: JsonValue| {
		let claims =
			json!({ "sub": "alice", "email": "alice@example.org", "email_verified": claim });

		let user_info: UserInfo =
			from_value(claims).expect("any email_verified claim deserializes");

		user_info.email_verified
	};

	assert_eq!(read(json!(true)), Some(true));
	assert_eq!(read(json!(false)), Some(false));
	assert_eq!(read(json!("true")), Some(true));
	assert_eq!(read(json!("True")), Some(true));
	assert_eq!(read(json!("false")), Some(false));
	assert_eq!(read(json!("False")), Some(false));
	assert_eq!(read(json!("yes")), Some(false));
	assert_eq!(read(json!(1)), Some(false));
	assert_eq!(read(json!({ "verified": true })), Some(false));
	assert_eq!(read(JsonValue::Null), None);

	let absent: UserInfo = from_value(json!({ "sub": "alice" })).expect("claim may be absent");

	assert_eq!(absent.email_verified, None);
}

#[test]
fn verified_email_follows_the_claim_then_trust() {
	let user_info = |email_verified| UserInfo {
		email: Some("alice@example.org".to_owned()),
		email_verified,
		..Default::default()
	};

	assert_eq!(user_info(Some(true)).verified_email(false), Some("alice@example.org"));
	assert_eq!(user_info(Some(false)).verified_email(true), None);
	assert_eq!(user_info(None).verified_email(true), Some("alice@example.org"));
	assert_eq!(user_info(None).verified_email(false), None);
	assert_eq!(UserInfo::default().verified_email(true), None);
}

/// A session stored before `email_verified` existed still decodes.
///
/// Sessions keep their userinfo CBOR-encoded, so the field's serde default is
/// what lets an old blob decode, and every value survives a round trip.
#[test]
fn stored_userinfo_decodes_with_or_without_email_verified() {
	// {"sub": "alice"}, as written before the claim was read.
	let legacy = [0xA1, 0x63, b's', b'u', b'b', 0x65, b'a', b'l', b'i', b'c', b'e'];
	let Cbor(decoded): Cbor<UserInfo> =
		deserialize_from_slice(&legacy).expect("legacy userinfo decodes");

	assert_eq!(decoded.sub, "alice");
	assert_eq!(decoded.email_verified, None);

	for email_verified in [Some(true), Some(false), None] {
		let user_info = UserInfo { email_verified, ..Default::default() };
		let encoded = serialize_to_vec(Cbor(&user_info)).expect("userinfo encodes");
		let Cbor(decoded): Cbor<UserInfo> =
			deserialize_from_slice(&encoded).expect("userinfo decodes");

		assert_eq!(decoded.email_verified, email_verified);
	}
}
