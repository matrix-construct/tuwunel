//! The MindRoom edit purge composes with upstream's `bundle_edit_relations`
//! (MSC3925).
//!
//! The purge deletes every superseded `m.replace` edit and keeps exactly one
//! per (target, sender). Upstream bundles the newest surviving edit onto the
//! original at `unsigned.m.relations.m.replace`, read from the
//! `relatesto_typed` typed index. The purge does NOT maintain that index (nor
//! `tofrom_relation`), so it leaves dangling rows for the edits it deletes.
//! These tests prove the two features compose:
//!
//!   1. after a real purge cycle deletes the superseded edit, a history
//!      endpoint still bundles the surviving edit; and
//!   2. a dangling *newest* typed-index row (its PDU purged) is skipped so the
//!      bundler falls through to the newest surviving edit — never serving
//!      stale pre-edit content, never erroring.
//!
//! The pre-purge assertions also pin the fork default `bundle_edit_relations =
//! true`: if that default were reverted these tests would fail with no bundle.

mod support;

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use axum::{Router, body::Body};
	use serde_json::{Value as JsonValue, json};
	use tower::ServiceExt;
	use tuwunel_core::{
		Result,
		http::{Request, StatusCode, header},
		ruma::user_id,
	};
	use tuwunel_service::Services;

	use super::support::Harness;

	const ALICE_TOKEN: &str = "mindroom-test-access-token-alice-0123456789";
	const BOB_TOKEN: &str = "mindroom-test-access-token-bob-0123456789ab";

	/// One harness per process (the tracing subscriber is global).
	#[test]
	fn edit_purge_composes_with_edit_bundling() -> Result {
		let harness = Harness::new("mindroom_edit_purge_bundle_compose", [
			"mindroom_edit_purge_enabled=true".to_owned(),
			// Make every superseded edit immediately eligible for purge.
			"mindroom_edit_purge_min_age_secs=0".to_owned(),
		])?;

		harness.with_services(|services| async move {
			let (router, room_id) = setup_room(&services).await?;

			real_purge_still_bundles_survivor(&services, &router, &room_id).await?;
			dangling_newest_falls_through(&services, &router, &room_id).await?;

			Ok(())
		})
	}

	/// A real purge cycle deletes the superseded edit and keeps the newest; the
	/// original still bundles the surviving edit afterwards, and the dangling
	/// typed-index row the purge leaves for the deleted edit does not matter
	/// (it is older than the survivor, so newest-first never reaches it).
	async fn real_purge_still_bundles_survivor(
		services: &Arc<Services>,
		router: &Router,
		room_id: &str,
	) -> Result {
		let original = send_text(router, room_id, ALICE_TOKEN, "compose-m1", "thinking…").await;
		let edit1 =
			send_edit(router, room_id, ALICE_TOKEN, "compose-e1", &original, "first answer")
				.await;
		let edit2 =
			send_edit(router, room_id, ALICE_TOKEN, "compose-e2", &original, "final answer")
				.await;

		// Before the purge: the original bundles the newest edit.
		let chunk = messages_chunk(router, &enc(room_id), ALICE_TOKEN).await;
		assert_edit_bundle(replace_bundle(find_event(&chunk, &original)), &edit2, "final answer");

		// Run one real purge cycle. It keeps edit2 and deletes edit1, leaving
		// edit1's relatesto_typed / tofrom_relation rows dangling.
		services.edit_purge.purge_cycle().await?;

		assert!(
			get_pdu_id(services, &edit1).await.is_none(),
			"purge must delete the superseded edit"
		);
		assert!(get_pdu_id(services, &edit2).await.is_some(), "purge must keep the newest edit");

		// After the purge: the original still bundles the surviving edit via the
		// typed index. This is the whole point — the purge does not need to
		// maintain relatesto_typed for history to stay correct.
		let chunk = messages_chunk(router, &enc(room_id), ALICE_TOKEN).await;
		assert_edit_bundle(replace_bundle(find_event(&chunk, &original)), &edit2, "final answer");

		Ok(())
	}

	/// If the *newest* typed-index row is dangling (its PDU deleted the way the
	/// purge deletes events), the bundler skips it and falls through to the
	/// newest surviving edit instead of erroring or serving nothing.
	async fn dangling_newest_falls_through(
		services: &Arc<Services>,
		router: &Router,
		room_id: &str,
	) -> Result {
		let original = send_text(router, room_id, ALICE_TOKEN, "compose-m2", "thinking…").await;
		let edit1 =
			send_edit(router, room_id, ALICE_TOKEN, "compose-f1", &original, "draft one").await;
		let edit2 =
			send_edit(router, room_id, ALICE_TOKEN, "compose-f2", &original, "draft two").await;

		// Delete only the newest edit's PDU rows, as the purge's `delete_event`
		// does, leaving its typed-index row dangling as the newest candidate.
		// (Nothing is sent after this: edit2 is the room's forward extremity and
		// removing its PDU would break prev_event resolution of any later send.)
		purge_event_rows(services, &edit2).await?;

		let chunk = messages_chunk(router, &enc(room_id), ALICE_TOKEN).await;
		// The dangling newest (edit2) is skipped; edit1 is the surviving edit.
		assert_edit_bundle(replace_bundle(find_event(&chunk, &original)), &edit1, "draft one");

		Ok(())
	}

	async fn setup_room(services: &Arc<Services>) -> Result<(Router, String)> {
		let alice = user_id!("@alice:localhost");
		let bob = user_id!("@bob:localhost");
		for (user, token) in [(alice, ALICE_TOKEN), (bob, BOB_TOKEN)] {
			services
				.users
				.create(user, Some("password"), None)
				.await?;
			services
				.users
				.create_device(user, None, (Some(token), None), None, None, None)
				.await?;
		}

		let (state, _guard) = tuwunel_api::router::state::create(services.clone());
		let router =
			tuwunel_api::router::build(Router::new(), &services.server).with_state(state);

		let body = request(
			&router,
			"POST",
			"/_matrix/client/v3/createRoom",
			ALICE_TOKEN,
			Some(json!({"preset": "public_chat"})),
		)
		.await;
		let room_id = body["room_id"]
			.as_str()
			.expect("createRoom returns room_id")
			.to_owned();

		request(
			&router,
			"POST",
			&format!("/_matrix/client/v3/rooms/{}/join", enc(&room_id)),
			BOB_TOKEN,
			Some(json!({})),
		)
		.await;

		Ok((router, room_id))
	}

	/// Resolve an event's PduId, or `None` if its PDU rows are gone.
	async fn get_pdu_id(services: &Arc<Services>, event_id: &str) -> Option<Vec<u8>> {
		services
			.timeline
			.get_pdu_id(event_id.try_into().expect("valid event id"))
			.await
			.ok()
			.map(|id| id.as_bytes().to_vec())
	}

	/// Delete an event's PDU rows the way the purge's `delete_event` does,
	/// leaving its typed-index (`relatesto_typed`) and `tofrom_relation` rows
	/// dangling. The bundler resolves a candidate row to a PDU via
	/// `pduid_pdu`, so removing that row is what makes the row dangle.
	async fn purge_event_rows(services: &Arc<Services>, event_id: &str) -> Result {
		let pdu_id = services
			.timeline
			.get_pdu_id(event_id.try_into().expect("valid event id"))
			.await?;
		services.db["pduid_pdu"].remove(pdu_id.as_bytes());
		services.db["eventid_pduid"].remove(event_id.as_bytes());

		Ok(())
	}

	async fn messages_chunk(router: &Router, room: &str, token: &str) -> Vec<JsonValue> {
		let messages = request(
			router,
			"GET",
			&format!("/_matrix/client/v3/rooms/{room}/messages?dir=b&limit=100"),
			token,
			None,
		)
		.await;

		messages["chunk"]
			.as_array()
			.expect("messages chunk")
			.clone()
	}

	/// Upstream serializes the bundled edit as a sync-shaped message-like event
	/// (`AnySyncMessageLikeEvent`): it carries `event_id`, `sender`, `type`,
	/// `origin_server_ts`, and the `m.new_content` edit payload, but not
	/// `room_id` (the parent event already carries the room context).
	fn assert_edit_bundle(bundle: &JsonValue, edit_id: &str, new_body: &str) {
		assert_eq!(
			bundle["event_id"], *edit_id,
			"bundle must be the surviving same-sender edit: {bundle}"
		);
		assert_eq!(bundle["sender"], "@alice:localhost", "bundle sender: {bundle}");
		assert_eq!(bundle["type"], "m.room.message", "bundle type: {bundle}");
		assert!(
			bundle["origin_server_ts"].is_u64(),
			"bundle must carry origin_server_ts (MindRoom Cinny requires it): {bundle}"
		);
		assert_eq!(
			bundle["content"]["m.new_content"]["body"], *new_body,
			"bundle content must carry the edited m.new_content: {bundle}"
		);
		assert_eq!(
			bundle["content"]["m.relates_to"]["rel_type"], "m.replace",
			"bundle content must retain m.relates_to: {bundle}"
		);
	}

	fn replace_bundle(event: &JsonValue) -> &JsonValue {
		let bundle = &event["unsigned"]["m.relations"]["m.replace"];
		assert!(bundle.is_object(), "event must carry an m.replace bundle: {event}");
		bundle
	}

	fn find_event<'a>(chunk: &'a [JsonValue], event_id: &str) -> &'a JsonValue {
		chunk
			.iter()
			.find(|event| event["event_id"] == *event_id)
			.unwrap_or_else(|| panic!("event {event_id} not found in chunk"))
	}

	async fn send_text(
		router: &Router,
		room: &str,
		token: &str,
		txn_id: &str,
		body: &str,
	) -> String {
		send_message(router, room, token, txn_id, json!({"msgtype": "m.text", "body": body}))
			.await
	}

	async fn send_message(
		router: &Router,
		room: &str,
		token: &str,
		txn_id: &str,
		content: JsonValue,
	) -> String {
		let body = request(
			router,
			"PUT",
			&format!("/_matrix/client/v3/rooms/{room}/send/m.room.message/{txn_id}"),
			token,
			Some(content),
		)
		.await;

		body["event_id"]
			.as_str()
			.expect("send returns event_id")
			.to_owned()
	}

	async fn send_edit(
		router: &Router,
		room: &str,
		token: &str,
		txn_id: &str,
		target: &str,
		new_body: &str,
	) -> String {
		send_message(
			router,
			room,
			token,
			txn_id,
			json!({
				"msgtype": "m.text",
				"body": format!("* {new_body}"),
				"m.new_content": {"msgtype": "m.text", "body": new_body},
				"m.relates_to": {"rel_type": "m.replace", "event_id": target},
			}),
		)
		.await
	}

	async fn request(
		router: &Router,
		method: &str,
		uri: &str,
		token: &str,
		body: Option<JsonValue>,
	) -> JsonValue {
		let request = Request::builder()
			.method(method)
			.uri(uri)
			.header(header::AUTHORIZATION, format!("Bearer {token}"))
			.header(header::CONTENT_TYPE, "application/json")
			.header("X-Forwarded-For", "127.0.0.1")
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
		assert_eq!(
			status,
			StatusCode::OK,
			"{method} {uri} should succeed: {}",
			String::from_utf8_lossy(&bytes),
		);

		serde_json::from_slice(&bytes).expect("JSON response body")
	}

	/// Percent-encode a room/event ID for use as a URI path segment.
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
