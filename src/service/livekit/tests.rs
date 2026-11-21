use http::StatusCode;
use ruma::{room_id, user_id};
use serde_json::{Value as JsonValue, json};
use tuwunel_core::{
	Result,
	jwt::{Algorithm, DecodingKey, Validation, decode},
	utils::time::now_secs,
};

use super::{
	Conf,
	names::{derive, identity, room_name},
	room_service::{Method, Room, answer, created, failed},
	token::Grant,
};

const SECRET: &str = "livekit-test-secret";

// The room-name vector of MSC4195's appendix.
#[test]
fn room_name_matches_the_msc_vector() {
	let name = room_name(room_id!("!roomid:example.com"), "slot1234");

	assert_eq!(name.as_str(), "O8437W3+jmzMVjoIP3tNwbm+XxHQk2iKpOA7aqw3qSc");
}

// The participant-identity vector of MSC4195's appendix.
#[test]
fn identity_matches_the_msc_vector() {
	let identity = identity(user_id!("@alice:example.com"), "memberABC");

	assert_eq!(identity.as_str(), "M3VnsLXrkbxIDINXO64u8PZzFyD9ZYFiQULR2dshWYs");
}

#[test]
fn room_name_folds_the_unstable_call_slot() {
	let room_id = room_id!("!roomid:example.com");
	let folded = room_name(room_id, "m.call#ROOM");

	assert_eq!(folded, room_name(room_id, "m.call#room"));
	assert_eq!(folded, derive(room_id.as_str(), "m.call#room"));
}

/// Every slot but the unstable call slot hashes as it is.
///
/// Another letter case of the call slot and another call slot keep rooms of
/// their own, apart from the call slot's room and from each other.
#[test]
fn room_name_folds_nothing_else() {
	let room_id = room_id!("!roomid:example.com");
	let call = room_name(room_id, "m.call#room");
	let mixed = room_name(room_id, "m.call#Room");
	let standup = room_name(room_id, "m.call#standup");

	assert_ne!(mixed, call);
	assert_ne!(standup, call);
	assert_ne!(mixed, standup);
	assert_eq!(mixed, derive(room_id.as_str(), "m.call#Room"));
	assert_eq!(standup, derive(room_id.as_str(), "m.call#standup"));
}

#[test]
fn join_token_carries_the_msc_grants() {
	let token = conf()
		.join_token("participant-identity", "livekit-room")
		.expect("a join token signs");

	let claims = claims(&token);

	assert_eq!(claims["iss"], "livekit-test-key");
	assert_eq!(claims["sub"], "participant-identity");
	assert_eq!(lifetime(&claims), 3600);
	assert_eq!(
		claims["video"],
		json!({
			"room": "livekit-room",
			"roomCreate": false,
			"roomJoin": true,
			"canPublish": true,
			"canSubscribe": true,
			"canUpdateOwnMetadata": false,
			"hidden": false,
		})
	);
}

#[test]
fn api_token_grants_only_what_the_call_needs() {
	let token = conf()
		.api_token(Grant::create_room("livekit-room"))
		.expect("an API token signs");

	let claims = claims(&token);
	let video = &claims["video"];

	assert_eq!(claims.get("sub"), None);
	assert_eq!(lifetime(&claims), 300);
	assert_eq!(video["roomCreate"], true);
	assert_eq!(video.get("roomAdmin"), None);
	assert_eq!(video["roomJoin"], false);
	assert_eq!(video["canPublish"], false);
}

#[test]
fn roomservice_base_swaps_the_scheme_and_keeps_the_path() {
	for (url, located) in [
		(
			"wss://livekit.example.com",
			"https://livekit.example.com/twirp/livekit.RoomService/M",
		),
		("ws://127.0.0.1:7880", "http://127.0.0.1:7880/twirp/livekit.RoomService/M"),
		(
			"wss://example.com/livekit",
			"https://example.com/livekit/twirp/livekit.RoomService/M",
		),
		(
			"wss://example.com/livekit/",
			"https://example.com/livekit/twirp/livekit.RoomService/M",
		),
		("wss://example.com/a?b=c#d", "https://example.com/a/twirp/livekit.RoomService/M"),
		(
			"wss://user:pass@example.com/a",
			"https://example.com/a/twirp/livekit.RoomService/M",
		),
	] {
		let method = Method::new(url, "M").expect("a websocket URL has a RoomService");

		assert_eq!(method.url.as_str(), located, "{url}");
		assert_eq!(method.name, "M");
	}

	assert!(Method::new("https://livekit.example.com", "M").is_err(), "not a websocket");
}

/// Only an HTTP 404 with Twirp's `not_found` code reads as not found.
///
/// Any other failure, a 404 from another source included, becomes a 502, and
/// the error never repeats LiveKit's message text.
#[test]
fn roomservice_failures_are_not_found_or_bad_gateway() {
	let not_found = br#"{"code":"not_found","msg":"participant does not exist"}"#;
	let missing: Result<Room> = answer(StatusCode::NOT_FOUND, not_found);
	let missing = missing.expect_err("a missing participant is an error");

	assert!(failed("M", missing).is_not_found(), "not found passes through");

	for (status, body, expected_fragment) in [
		(
			StatusCode::NOT_FOUND,
			br#"{"code":"bad_route","msg":"text"}"#.as_slice(),
			"bad_route",
		),
		(StatusCode::NOT_FOUND, b"<html>text</html>", "404"),
		(
			StatusCode::UNAUTHORIZED,
			br#"{"code":"unauthenticated","msg":"text"}"#,
			"unauthenticated",
		),
		(
			StatusCode::BAD_GATEWAY,
			br#"{"code":"Not A Code","msg":"text"}"#,
			"unrecognized",
		),
		(StatusCode::OK, b"not json", ""),
		(StatusCode::OK, b"{}", ""),
	] {
		let result: Result<Room> = answer(status, body);
		let error = result.expect_err("the answer is a failure");
		let message = error.to_string();

		assert!(
			message.contains(expected_fragment) && !message.contains("text"),
			"{status}: {message}"
		);
		assert_eq!(failed("M", error).status_code(), StatusCode::BAD_GATEWAY, "{status}");
	}
}

/// A CreateRoom answer counts only when it names the requested room.
///
/// LiveKit answers with the whole room, so an answer carrying metadata well
/// past 64 KiB still parses.
#[test]
fn createroom_answer_must_name_the_room() {
	let name = "O8437W3+jmzMVjoIP3tNwbm+XxHQk2iKpOA7aqw3qSc";
	let metadata = "m".repeat(80 * 1024);
	let body = json!({"sid": "RM_x", "name": name, "metadata": metadata}).to_string();
	let room: Room = answer(StatusCode::OK, body.as_bytes()).expect("a large answer parses");

	created(&room, name).expect("the requested room was created");
	created(&room, "another-room").expect_err("an answer for another room is a failure");
}

fn conf() -> Conf {
	Conf::new(("wss://livekit.example.com", "livekit-test-key", SECRET))
		.expect("a websocket URL makes a configuration")
}

fn claims(token: &str) -> JsonValue {
	decode::<JsonValue>(
		token,
		&DecodingKey::from_secret(SECRET.as_bytes()),
		&Validation::new(Algorithm::HS256),
	)
	.expect("the token verifies with the API secret")
	.claims
}

fn lifetime(claims: &JsonValue) -> u64 {
	let nbf = claims["nbf"].as_u64().expect("nbf is a number");
	let exp = claims["exp"].as_u64().expect("exp is a number");

	assert!(nbf.abs_diff(now_secs()) <= 1, "nbf is now: {claims}");
	exp.checked_sub(nbf).expect("exp is after nbf")
}
