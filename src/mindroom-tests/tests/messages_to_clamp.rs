//! `/messages` must honour `to` as a *position*, not an exact event count.
//!
//! Upstream stopped pagination only when an event's count exactly equalled the
//! `to` token (`Some(*count) != to`) and silently swallowed unparseable `to`
//! values. A sync `next_batch` token is a global stream position that almost
//! never equals an event count *in the requested room*, so a `to` bound taken
//! from `/sync` — which the spec explicitly permits — was ignored entirely and
//! the walk ran unbounded. MindRoom's nio fork pages `/messages` with sync
//! tokens to recover gaps dropped by limited sync timelines, which is how the
//! bug was found (live probes: `to=<sync token>` and even `to=garbage!!`
//! returned HTTP 200 with an unclamped chunk).
//!
//! These tests pin the fix: pagination stops once the walk passes the `to`
//! position in either direction, and an unparseable `to` fails the request
//! (matching `from` handling) instead of being ignored.

mod support;

#[cfg(test)]
mod tests {
	use axum::{Router, body::Body};
	use serde_json::{Value as JsonValue, json};
	use tower::ServiceExt;
	use tuwunel_core::{
		Result,
		http::{Request, StatusCode, header},
		ruma::user_id,
	};

	use super::support::Harness;

	const ALICE_TOKEN: &str = "mindroom-test-access-token-alice-0123456789";

	/// One harness per process (the tracing subscriber is global).
	#[test]
	fn messages_to_clamps_at_positions_between_events() -> Result {
		let harness = Harness::new("mindroom_messages_to_clamp", [])?;

		harness.with_services(|services| async move {
			let alice = user_id!("@alice:localhost");
			services
				.users
				.create(alice, Some("password"), None)
				.await?;
			services
				.users
				.create_device(alice, None, (Some(ALICE_TOKEN), None), None, None, None)
				.await?;

			let (state, _guard) = tuwunel_api::router::state::create(services.clone());
			let router =
				tuwunel_api::router::build(Router::new(), &services.server).with_state(state);

			let room_a = create_room(&router).await;

			// Two messages before the position, three after. The position is a
			// sync next_batch taken while another room advances the global
			// stream, so it falls strictly *between* room A's events and equals
			// none of their counts — exactly the shape of a sync token used as
			// a pagination bound.
			send_text(&router, &room_a, "m1").await;
			send_text(&router, &room_a, "m2").await;
			let _room_b = create_room(&router).await;
			let position = sync_next_batch(&router).await;
			send_text(&router, &room_a, "m3").await;
			send_text(&router, &room_a, "m4").await;
			send_text(&router, &room_a, "m5").await;

			backward_walk_stops_at_position(&router, &room_a, &position).await;
			forward_walk_starts_at_position(&router, &room_a, &position).await;
			unparseable_to_fails_the_request(&router, &room_a).await;

			Ok(())
		})
	}

	/// dir=b from the room's head with `to=<position>` must return only the
	/// events after the position and stop — not run past it to the room's
	/// creation (the pre-fix behaviour, since no event count equals it).
	async fn backward_walk_stops_at_position(router: &Router, room: &str, position: &str) {
		let (status, body) = request(
			router,
			"GET",
			&format!(
				"/_matrix/client/v3/rooms/{}/messages?dir=b&limit=100&to={position}",
				enc(room)
			),
			Some(ALICE_TOKEN),
			None,
		)
		.await;

		assert_eq!(status, StatusCode::OK, "clamped backward walk should succeed: {body}");
		assert_eq!(
			bodies(&body),
			vec!["m5", "m4", "m3"],
			"backward walk must stop at the `to` position: {body}"
		);
	}

	/// dir=f from the position returns exactly the events after it; together
	/// with the backward assertion this pins that the two directions partition
	/// the room at the position.
	async fn forward_walk_starts_at_position(router: &Router, room: &str, position: &str) {
		let (status, body) = request(
			router,
			"GET",
			&format!(
				"/_matrix/client/v3/rooms/{}/messages?dir=f&limit=100&from={position}",
				enc(room)
			),
			Some(ALICE_TOKEN),
			None,
		)
		.await;

		assert_eq!(status, StatusCode::OK, "forward walk from position should succeed: {body}");
		assert_eq!(
			bodies(&body),
			vec!["m3", "m4", "m5"],
			"forward walk must start at the position: {body}"
		);
	}

	/// An unparseable `to` must fail the request like an unparseable `from`
	/// does, not be silently ignored (the pre-fix behaviour returned HTTP 200
	/// with an unbounded chunk).
	async fn unparseable_to_fails_the_request(router: &Router, room: &str) {
		let (status, body) = request(
			router,
			"GET",
			&format!("/_matrix/client/v3/rooms/{}/messages?dir=b&limit=100&to=garbage!!", enc(room)),
			Some(ALICE_TOKEN),
			None,
		)
		.await;

		assert!(
			status.is_client_error() || status.is_server_error(),
			"unparseable `to` must fail the request, got {status}: {body}"
		);
	}

	async fn create_room(router: &Router) -> String {
		let (status, body) = request(
			router,
			"POST",
			"/_matrix/client/v3/createRoom",
			Some(ALICE_TOKEN),
			Some(json!({"preset": "public_chat"})),
		)
		.await;
		assert_eq!(status, StatusCode::OK, "createRoom should succeed: {body}");

		body["room_id"]
			.as_str()
			.expect("createRoom returns room_id")
			.to_owned()
	}

	async fn send_text(router: &Router, room: &str, text: &str) -> String {
		let (status, body) = request(
			router,
			"PUT",
			&format!("/_matrix/client/v3/rooms/{}/send/m.room.message/txn-{text}", enc(room)),
			Some(ALICE_TOKEN),
			Some(json!({"msgtype": "m.text", "body": text})),
		)
		.await;
		assert_eq!(status, StatusCode::OK, "send should succeed: {body}");

		body["event_id"]
			.as_str()
			.expect("send returns event_id")
			.to_owned()
	}

	async fn sync_next_batch(router: &Router) -> String {
		let (status, body) = request(
			router,
			"GET",
			"/_matrix/client/v3/sync?timeout=0",
			Some(ALICE_TOKEN),
			None,
		)
		.await;
		assert_eq!(status, StatusCode::OK, "sync should succeed: {body}");

		body["next_batch"]
			.as_str()
			.expect("sync returns next_batch")
			.to_owned()
	}

	fn bodies(messages: &JsonValue) -> Vec<&str> {
		messages["chunk"]
			.as_array()
			.expect("messages chunk")
			.iter()
			.filter(|event| event["type"] == "m.room.message")
			.filter_map(|event| event["content"]["body"].as_str())
			.collect()
	}

	async fn request(
		router: &Router,
		method: &str,
		uri: &str,
		token: Option<&str>,
		body: Option<JsonValue>,
	) -> (StatusCode, JsonValue) {
		let mut builder = Request::builder()
			.method(method)
			.uri(uri)
			.header(header::CONTENT_TYPE, "application/json")
			.header("X-Forwarded-For", "127.0.0.1");
		if let Some(token) = token {
			builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
		}
		let request = builder
			.body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
			.expect("valid request");

		let response = router
			.clone()
			.oneshot(request)
			.await
			.expect("router response");

		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
			.await
			.expect("readable response body");

		(status, serde_json::from_slice(&bytes).unwrap_or(JsonValue::Null))
	}

	/// Percent-encode a room ID for use as a URI path segment.
	fn enc(id: &str) -> String {
		id.bytes()
			.map(|byte| match byte {
				| b'$' => "%24".to_owned(),
				| b'!' => "%21".to_owned(),
				| b':' => "%3A".to_owned(),
				| b'+' => "%2B".to_owned(),
				| b'/' => "%2F".to_owned(),
				| b'=' => "%3D".to_owned(),
				| b'?' => "%3F".to_owned(),
				| b'#' => "%23".to_owned(),
				| other => char::from(other).to_string(),
			})
			.collect()
	}
}
