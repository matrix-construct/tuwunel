use std::{
	net::{IpAddr, Ipv4Addr},
	time::{Duration, Instant},
};

use futures::TryStreamExt;
use ruma::user_id;
use tuwunel_core::{Result, config::Figment, result::NotFound};

use super::{Deletions, Ratelimiter, Session, UserInfo, check_bucket_at};
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

	let sentinel = |field: &str| format!("{field}-sentinel");
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

	for field in secrets {
		assert!(!dump.contains(&sentinel(field)), "{field} leaked in Debug: {dump}");
		assert!(
			dump.contains(&format!("{field}: Some(<redacted>)")),
			"{field} should appear as Some(<redacted>): {dump}"
		);
	}
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
