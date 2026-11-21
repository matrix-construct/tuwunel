use http::{Response, StatusCode};
use ruma::{
	api::{
		EndpointError,
		client::rtc::livekit::get_token::v1::Request,
		error::{Error as RumaError, ErrorKind},
	},
	room_id, server_name, user_id,
};
use serde_json::{Value as JsonValue, json};
use tuwunel_core::{
	Error, Result, err,
	jwt::{Algorithm, DecodingKey, Validation, decode},
	utils::time::now_secs,
};

use super::{
	Conf,
	delegation::{Action, Record, decide},
	get_token::{is_unrecognized, relay_error, stable_request, unstable_request},
	names::{derive, identity, room_name},
	room_service::{Method, Presence, Room, answer, created, failed, not_created, presence},
	token::Grant,
};
use crate::delayed_events::Event;

const SECRET: &str = "livekit-test-secret";
const URL: &str = "wss://livekit.example.com";

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

/// CreateRoom's token may create rooms and GetParticipant's may administer
/// the one room it names.
///
/// Neither token names a participant or permits joining.
#[test]
fn api_token_grants_only_what_the_call_needs() {
	for (grant, create, admin) in [
		(Grant::create_room("livekit-room"), true, None),
		(Grant::room_admin("livekit-room"), false, Some(true)),
	] {
		let token = conf()
			.api_token(grant)
			.expect("an API token signs");

		let claims = claims(&token);
		let video = &claims["video"];
		let room_admin = video
			.get("roomAdmin")
			.and_then(JsonValue::as_bool);

		assert_eq!(claims.get("sub"), None);
		assert_eq!(lifetime(&claims), 300);
		assert_eq!(video["room"], "livekit-room");
		assert_eq!(video["roomCreate"], create);
		assert_eq!(room_admin, admin);
		assert_eq!(video["roomJoin"], false);
		assert_eq!(video["canPublish"], false);
	}
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
/// past 64 KiB still parses. CreateRoom has no not-found answer, so one is a
/// 502 like any other failure.
#[test]
fn createroom_answer_must_name_the_room() {
	let name = "O8437W3+jmzMVjoIP3tNwbm+XxHQk2iKpOA7aqw3qSc";
	let metadata = "m".repeat(80 * 1024);
	let body = json!({"sid": "RM_x", "name": name, "metadata": metadata}).to_string();
	let room: Room = answer(StatusCode::OK, body.as_bytes()).expect("a large answer parses");

	created(&room, name).expect("the requested room was created");
	created(&room, "another-room").expect_err("an answer for another room is a failure");

	let not_found = br#"{"code":"not_found","msg":"room does not exist"}"#;
	let missing: Result<Room> = answer(StatusCode::NOT_FOUND, not_found);
	let missing = missing.expect_err("a not-found answer is an error");

	assert_eq!(not_created("CreateRoom", missing).status_code(), StatusCode::BAD_GATEWAY);
}

/// LiveKit's participant states read as active, joining or gone.
///
/// A participant LiveKit does not know is gone like a disconnected one, and
/// an answer leaving out the state reads as protobuf's zero value, joining.
/// Any other failure stays an error, on which a poll changes nothing.
#[test]
fn participant_states_read_as_presence() {
	let not_found = r#"{"code":"not_found","msg":"participant does not exist"}"#;

	for (status, body, expected) in [
		(StatusCode::OK, r#"{"identity":"x","state":"ACTIVE"}"#, Some(Presence::Active)),
		(StatusCode::OK, r#"{"state":"JOINED"}"#, Some(Presence::Joining)),
		(StatusCode::OK, r#"{"state":"JOINING"}"#, Some(Presence::Joining)),
		(StatusCode::OK, r#"{"identity":"x"}"#, Some(Presence::Joining)),
		(StatusCode::OK, r#"{"state":"DISCONNECTED"}"#, Some(Presence::Gone)),
		(StatusCode::NOT_FOUND, not_found, Some(Presence::Gone)),
		(StatusCode::NOT_FOUND, "<html>text</html>", None),
		(StatusCode::UNAUTHORIZED, r#"{"code":"unauthenticated","msg":"text"}"#, None),
		(StatusCode::OK, r#"{"state":"UNHEARD_OF"}"#, None),
	] {
		let presence = presence(answer(status, body.as_bytes())).ok();

		assert_eq!(presence, expected, "{status} {body}");
	}
}

/// A connected participant's event restarts once it falls due within half its
/// delay plus one poll interval, and not a millisecond sooner.
///
/// A waiting one restarts only on connecting and is let go once its hour is
/// over, even while LiveKit cannot answer; an unanswered poll otherwise sends
/// and restarts nothing.
#[test]
fn decide_covers_every_row() {
	const SINCE: u64 = 1_000_000;
	const DELAY: u64 = 3_600_000;
	const EDGE: u64 = SINCE + DELAY - DELAY / 2 - 10_000;
	const WAITED: u64 = SINCE + DELAY;

	let event = scheduled(DELAY, SINCE + DELAY);
	let (waiting, connected) = (record(SINCE, false), record(SINCE, true));
	let (active, joining, gone) =
		(Some(Presence::Active), Some(Presence::Joining), Some(Presence::Gone));

	for (record, presence, now, expected) in [
		(&waiting, active, SINCE, Action::Connect { restart: false }),
		(&waiting, active, EDGE, Action::Connect { restart: true }),
		(&waiting, joining, WAITED - 1, Action::Wait),
		(&waiting, gone, WAITED - 1, Action::Wait),
		(&waiting, None, WAITED - 1, Action::Wait),
		(&waiting, joining, WAITED, Action::GiveUp),
		(&waiting, gone, WAITED, Action::GiveUp),
		(&waiting, None, WAITED, Action::GiveUp),
		(&connected, active, EDGE - 1, Action::Wait),
		(&connected, joining, EDGE - 1, Action::Wait),
		(&connected, active, EDGE, Action::Restart),
		(&connected, joining, EDGE, Action::Restart),
		(&connected, None, EDGE, Action::Wait),
		(&connected, gone, SINCE, Action::Send),
	] {
		let action = decide(record, presence, &event, now);

		assert_eq!(action, expected, "{record:?} {presence:?} {now}");
	}
}

/// Two delegations of the same event made in the same millisecond differ.
///
/// A poll that read one therefore never acts on the other.
#[test]
fn delegations_made_at_once_differ() {
	let user_id = user_id!("@alice:example.com");

	let made = || Record::new(user_id, "delay", URL, 5);

	assert_ne!(made(), made());
}

/// Only another server's 403 `M_FORBIDDEN` and 400 `M_INVALID_PARAM` reach
/// the client, each as this server's own refusal.
///
/// The errcode is read from the raw body, so a bare `{"errcode": ...}` counts.
/// Either errcode with another status, any other answer and a failure to reach
/// the server all become a 502 `M_UNKNOWN`, and none repeats the remote's text.
#[test]
fn relay_error_keeps_only_forbidden_and_invalid_param() {
	let forbidden = (StatusCode::FORBIDDEN, ErrorKind::Forbidden);
	let invalid = (StatusCode::BAD_REQUEST, ErrorKind::InvalidParam);
	let unknown = (StatusCode::BAD_GATEWAY, ErrorKind::Unknown);

	for (status, body, relayed) in [
		(
			StatusCode::FORBIDDEN,
			r#"{"errcode":"M_FORBIDDEN","error":"remote text"}"#,
			&forbidden,
		),
		(StatusCode::FORBIDDEN, r#"{"errcode":"M_FORBIDDEN"}"#, &forbidden),
		(
			StatusCode::BAD_REQUEST,
			r#"{"errcode":"M_INVALID_PARAM","error":"remote text"}"#,
			&invalid,
		),
		(
			StatusCode::INTERNAL_SERVER_ERROR,
			r#"{"errcode":"M_FORBIDDEN","error":"remote text"}"#,
			&unknown,
		),
		(
			StatusCode::FORBIDDEN,
			r#"{"errcode":"M_INVALID_PARAM","error":"remote text"}"#,
			&unknown,
		),
		(
			StatusCode::NOT_FOUND,
			r#"{"errcode":"M_NOT_FOUND","error":"remote text"}"#,
			&unknown,
		),
		(
			StatusCode::TOO_MANY_REQUESTS,
			r#"{"errcode":"M_LIMIT_EXCEEDED","error":"remote text"}"#,
			&unknown,
		),
		(StatusCode::BAD_GATEWAY, "remote text", &unknown),
	] {
		let error = relay_error(&remote(status, body));

		assert_eq!(&(error.status_code(), error.kind()), relayed, "{status} {body}");
		assert!(!error.message().contains("remote text"), "{error}");
	}

	let transport = relay_error(&err!("remote text: connection refused"));

	assert_eq!((transport.status_code(), transport.kind()), unknown);
}

/// A 404 triggers the unstable retry when its errcode is `M_UNRECOGNIZED`,
/// empty or missing, as lk-jwt-service reads it.
///
/// Any other errcode, any other status and a failure to reach the server
/// leave the stable answer standing.
#[test]
fn retry_only_on_an_unrecognized_404() {
	for (status, body, retried) in [
		(StatusCode::NOT_FOUND, r#"{"errcode":"M_UNRECOGNIZED","error":"text"}"#, true),
		(StatusCode::NOT_FOUND, r#"{"errcode":"","error":"text"}"#, true),
		(StatusCode::NOT_FOUND, "{}", true),
		(StatusCode::NOT_FOUND, "<html>text</html>", true),
		(StatusCode::NOT_FOUND, r#"{"errcode":"M_NOT_FOUND"}"#, false),
		(StatusCode::NOT_FOUND, r#"{"errcode":"M_NOT_FOUND","error":"text"}"#, false),
		(StatusCode::BAD_REQUEST, r#"{"errcode":"M_UNRECOGNIZED","error":"text"}"#, false),
	] {
		assert_eq!(is_unrecognized(&remote(status, body)), retried, "{status} {body}");
	}

	assert!(!is_unrecognized(&err!("connection refused")), "a transport failure");
}

/// The stable request carries the slot as sent; the unstable retry spells
/// the call slot as servers predating the fold hash it.
///
/// Every other field, and every other slot, goes out the same on both.
#[test]
fn unstable_retry_sends_the_unstable_call_slot() {
	let user_id = user_id!("@alice:example.com");

	for (slot, retried) in [
		("m.call#room", "m.call#ROOM"),
		("m.call#ROOM", "m.call#ROOM"),
		("m.call#standup", "m.call#standup"),
	] {
		let room_id = room_id!("!roomid:example.com").to_owned();
		let url = "wss://livekit.example.com".to_owned();
		let request = Request::new(url, room_id, slot.to_owned(), "memberABC".to_owned());
		let (stable, unstable) =
			(stable_request(user_id, &request), unstable_request(user_id, &request));

		assert_eq!((stable.slot_id.as_str(), unstable.slot_id.as_str()), (slot, retried));

		for (sent_user, url, room_id, member_id) in [
			(&stable.user_id, &stable.url, &stable.room_id, &stable.member_id),
			(&unstable.user_id, &unstable.url, &unstable.room_id, &unstable.member_id),
		] {
			assert_eq!(sent_user, user_id);
			assert_eq!(url, &request.url);
			assert_eq!(room_id, &request.room_id);
			assert_eq!(member_id, &request.member_id);
		}
	}
}

fn remote(status: StatusCode, body: &str) -> Error {
	let response = Response::builder()
		.status(status)
		.body(body.as_bytes())
		.expect("a response builds");

	Error::Federation(
		server_name!("remote.example.com").to_owned(),
		RumaError::from_http_response(response),
	)
}

fn scheduled(delay_ms: u64, due_ms: u64) -> Event {
	let event = json!({
		"user_id": "@alice:example.com",
		"room_id": "!roomid:example.com",
		"type": "org.matrix.msc3401.call.member",
		"content": {},
		"delay_ms": delay_ms,
		"due_ms": due_ms,
	});

	serde_json::from_value(event).expect("a scheduled event deserializes")
}

fn record(since_ms: u64, connected: bool) -> Record {
	let user_id = user_id!("@alice:example.com");

	Record {
		connected,
		..Record::new(user_id, "delay", URL, since_ms)
	}
}

fn conf() -> Conf {
	Conf::new((URL, "livekit-test-key", SECRET)).expect("a websocket URL makes a configuration")
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
