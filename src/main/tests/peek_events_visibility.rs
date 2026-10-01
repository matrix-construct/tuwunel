#![cfg(test)]

use reqwest::{Response, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{
	Result, err, implement,
	ruma::{OwnedRoomId, RoomId, UserId},
};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
};

mod client;
mod fixture;

const OWNER_TOKEN: &str = "peek-events-visibility-owner-access-token";
const MEMBER_TOKEN: &str = "peek-events-visibility-member-access-token";
const STRANGER_TOKEN: &str = "peek-events-visibility-stranger-access-token";

/// The room event stream serves only events the requester may see.
///
/// A member reads nothing from before its history visibility admits it, apart
/// from its own membership event that does. A removed member is refused
/// outright, and a peek into a world-readable room skips what came before it
/// became world-readable. Without a `from` token the stream starts now.
#[test]
fn room_event_stream_honours_history_visibility() -> Result {
	boot("peek-events-visibility", ["client_sync_timeout_min=0"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let owner_id = register(services, "peekowner", OWNER_TOKEN).await?;

	register(services, "peekstranger", STRANGER_TOKEN).await?;

	let member_id = register(services, "peekmember", MEMBER_TOKEN).await?;

	let owner = Client { services, base, token: OWNER_TOKEN };
	let member = Client { services, base, token: MEMBER_TOKEN };
	let stranger = Client { services, base, token: STRANGER_TOKEN };

	member_reads_from_its_invite(&owner, &member, &member_id).await?;
	member_sees_its_own_join(&owner, &owner_id, &member, &member_id).await?;
	member_sees_its_own_uninvited_join(&owner, &member, &member_id).await?;
	banned_member_is_refused(&owner, &member, &member_id).await?;
	peek_skips_history_before_world_readable(&owner, &member, &stranger).await
}

/// A member of an `invited` room reads its stream from the invite onward.
///
/// An invitee is refused until it joins. The same room covers the token
/// handling: a malformed one is refused, and an absent one waits for events
/// newer than the request.
async fn member_reads_from_its_invite(
	owner: &Client<'_>,
	member: &Client<'_>,
	member_id: &UserId,
) -> Result {
	let room_id = owner
		.create_room_with_history("private_chat", "invited")
		.await?;

	owner.send_text(&room_id, "before-invite").await?;

	owner
		.act_on(&room_id, "invite", member_id)
		.await?;

	let invited = member.events(&room_id, None).await?;

	assert_eq!(invited.status(), StatusCode::FORBIDDEN);

	member.act(&room_id, "join").await?;
	owner.send_text(&room_id, "after-join").await?;

	let replay = member.page(&room_id, Some("0")).await?;

	assert_eq!(bodies(&replay), ["after-join"]);
	assert_eq!(memberships(&replay, member_id), ["invite", "join"]);

	let malformed = member
		.events(&room_id, Some("not-a-token"))
		.await?;

	assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);

	let idle = member.page(&room_id, None).await?;

	assert!(
		idle.get("chunk")
			.and_then(Value::as_array)
			.is_none_or(Vec::is_empty)
	);

	let end = field(&idle, "end")?;

	owner.send_text(&room_id, "live").await?;

	let live = member.page(&room_id, Some(end)).await?;

	assert_eq!(bodies(&live), ["live"]);

	Ok(())
}

/// A member of a `joined` room sees its own join but none of the history sent
/// under that setting before it.
///
/// The join is admitted by the membership it sets. Its invite is not admitted,
/// since an invite does not satisfy `joined` history. The owner's rename is not
/// admitted either, since only the member's own events are judged by their
/// outcome. The room's opening events predate the `joined` setting and stay
/// visible.
async fn member_sees_its_own_join(
	owner: &Client<'_>,
	owner_id: &UserId,
	member: &Client<'_>,
	member_id: &UserId,
) -> Result {
	let room_id = owner
		.create_room_with_history("private_chat", "joined")
		.await?;

	owner
		.send_text(&room_id, "before-own-join")
		.await?;

	let rename = json!({ "membership": "join", "displayname": "renamed-before-own-join" });

	owner
		.put(&format!("rooms/{room_id}/state/m.room.member/{owner_id}"), &rename)
		.await?;

	owner
		.act_on(&room_id, "invite", member_id)
		.await?;

	member.act(&room_id, "join").await?;

	let replay = member.page(&room_id, Some("0")).await?;

	assert!(!bodies(&replay).contains(&"before-own-join"));
	assert_eq!(memberships(&replay, owner_id), ["join"]);
	assert_eq!(memberships(&replay, member_id), ["join"]);

	Ok(())
}

/// A member joining an `invited` room without an invite sees its own join.
///
/// Its membership before the join admits nothing under `invited` history, so
/// only the membership the join sets can admit it. The single-event lookup
/// serves the join through its own room and refuses it through another room
/// the member belongs to.
async fn member_sees_its_own_uninvited_join(
	owner: &Client<'_>,
	member: &Client<'_>,
	member_id: &UserId,
) -> Result {
	let room_id = owner
		.create_room_with_history("public_chat", "invited")
		.await?;

	owner
		.send_text(&room_id, "before-uninvited-join")
		.await?;

	member.act(&room_id, "join").await?;

	let replay = member.page(&room_id, Some("0")).await?;

	assert!(!bodies(&replay).contains(&"before-uninvited-join"));
	assert_eq!(memberships(&replay, member_id), ["join"]);

	let join_id = member_events(&replay, member_id)
		.find_map(|event| event["event_id"].as_str())
		.ok_or_else(|| err!("page omitted the member's join"))?;

	let elsewhere = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	member.act(&elsewhere, "join").await?;

	let own_room = member
		.get(&format!("rooms/{room_id}/event/{join_id}"))
		.await?;

	let other_room = member
		.get(&format!("rooms/{elsewhere}/event/{join_id}"))
		.await?;

	assert_eq!(own_room.status(), StatusCode::OK);
	assert_eq!(other_room.status(), StatusCode::NOT_FOUND);

	Ok(())
}

/// A member banned from a `shared` room may no longer stream it.
///
/// Its retained once-joined record must not stand in for a peek, since the
/// stream would otherwise keep delivering the room's new events.
async fn banned_member_is_refused(
	owner: &Client<'_>,
	member: &Client<'_>,
	member_id: &UserId,
) -> Result {
	let room_id = owner
		.create_room(&json!({ "preset": "private_chat" }))
		.await?;

	owner
		.act_on(&room_id, "invite", member_id)
		.await?;

	member.act(&room_id, "join").await?;
	owner.act_on(&room_id, "ban", member_id).await?;

	let refused = member.events(&room_id, None).await?;

	assert_eq!(refused.status(), StatusCode::FORBIDDEN);

	Ok(())
}

/// A peek into a room sees only what was sent while it was world-readable.
///
/// The room starts `shared`, so the message sent before the switch stays
/// hidden from a stranger. A former member who was joined when it was sent is
/// refused it too, since a peek is served as a room preview.
async fn peek_skips_history_before_world_readable(
	owner: &Client<'_>,
	member: &Client<'_>,
	stranger: &Client<'_>,
) -> Result {
	let room_id = owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	member.act(&room_id, "join").await?;

	owner
		.send_text(&room_id, "before-world-readable")
		.await?;

	member.act(&room_id, "leave").await?;

	let path = format!("rooms/{room_id}/state/m.room.history_visibility");

	owner
		.put(&path, &json!({ "history_visibility": "world_readable" }))
		.await?;

	owner
		.send_text(&room_id, "world-readable")
		.await?;

	let stranger_peek = stranger.page(&room_id, Some("0")).await?;

	assert_eq!(bodies(&stranger_peek), ["world-readable"]);
	assert!(opens_history(&stranger_peek));

	let former_peek = member.page(&room_id, Some("0")).await?;

	assert_eq!(bodies(&former_peek), ["world-readable"]);
	assert!(opens_history(&former_peek));

	Ok(())
}

/// Create a room from a preset with its history visibility set from the start.
///
/// The setting lands among the room's opening state, so those opening events
/// were sent under `shared` and only what follows falls under the setting.
#[implement(Client, params = "<'_>")]
async fn create_room_with_history(
	&self,
	preset: &str,
	history_visibility: &str,
) -> Result<OwnedRoomId> {
	self.create_room(&json!({
		"preset": preset,
		"initial_state": [{
			"type": "m.room.history_visibility",
			"state_key": "",
			"content": { "history_visibility": history_visibility },
		}],
	}))
	.await
}

/// Send a text message, using its body as the transaction id.
///
/// Every body the test sends is distinct, which keeps the transaction ids
/// distinct as well.
#[implement(Client, params = "<'_>")]
async fn send_text(&self, room_id: &RoomId, body: &str) -> Result {
	let path = format!("rooms/{room_id}/send/m.room.message/{body}");

	self.put(&path, &json!({ "msgtype": "m.text", "body": body }))
		.await
}

/// Get one endpoint path as this user and keep the raw reply.
///
/// The status stays the caller's to judge, so a refusal is as easy to assert as
/// a success.
#[implement(Client, params = "<'_>")]
async fn get(&self, path: &str) -> Result<Response> {
	let response = self
		.services
		.client
		.clients
		.default
		.get(self.url(path))
		.bearer_auth(self.token)
		.send()
		.await?;

	Ok(response)
}

/// Put a JSON body to one endpoint path as this user.
///
/// A non-success status is the error, as with the harness's `post`.
#[implement(Client, params = "<'_>")]
async fn put(&self, path: &str, body: &Value) -> Result {
	self.services
		.client
		.clients
		.default
		.put(self.url(path))
		.bearer_auth(self.token)
		.json(body)
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

/// Apply a membership action naming another user, such as an invite or a ban.
///
/// The membership is written before the response returns, so a request that
/// depends on it can follow directly.
#[implement(Client, params = "<'_>")]
async fn act_on(&self, room_id: &RoomId, action: &str, user_id: &UserId) -> Result {
	let path = format!("rooms/{room_id}/{action}");

	self.post(&path, &json!({ "user_id": user_id }))
		.await
		.map(drop)
}

/// Apply a membership action to this user, such as a join or a leave.
///
/// The membership is written before the response returns, so every request
/// that follows sees it.
#[implement(Client, params = "<'_>")]
async fn act(&self, room_id: &RoomId, action: &str) -> Result {
	let path = format!("rooms/{room_id}/{action}");

	self.post(&path, &json!({})).await.map(drop)
}

/// Read one page of the room's event stream as this user.
///
/// A non-success status is the error, so the caller only sees an accepted
/// page.
#[implement(Client, params = "<'_>")]
async fn page(&self, room_id: &RoomId, from: Option<&str>) -> Result<Value> {
	let page = self
		.events(room_id, from)
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(page)
}

/// Request one page of the room's event stream without waiting.
///
/// The server boots with no minimum timeout, so a zero timeout answers at once
/// with whatever is already there. The status stays the caller's to judge.
#[implement(Client, params = "<'_>")]
async fn events(&self, room_id: &RoomId, from: Option<&str>) -> Result<Response> {
	let query = [("room_id", Some(room_id.as_str())), ("timeout", Some("0")), ("from", from)];

	let response = self
		.services
		.client
		.clients
		.default
		.get(self.url("events"))
		.bearer_auth(self.token)
		.query(&query)
		.send()
		.await?;

	Ok(response)
}

/// The message bodies a page carries, in stream order.
///
/// Events without a string `content.body`, state events among them, are
/// skipped.
fn bodies(page: &Value) -> Vec<&str> {
	chunk(page)
		.filter_map(|event| event.pointer("/content/body"))
		.filter_map(Value::as_str)
		.collect()
}

/// The memberships a page's member events set for one user, in stream order.
///
/// A profile change repeats the membership it keeps, so a renamed member shows
/// `join` once per rename as well as for the join itself.
fn memberships<'a>(page: &'a Value, user_id: &UserId) -> Vec<&'a str> {
	member_events(page, user_id)
		.filter_map(|event| event.pointer("/content/membership"))
		.filter_map(Value::as_str)
		.collect()
}

/// The member events a page carries about one user, in stream order.
///
/// Events about any other user, and every non-member event, are skipped.
fn member_events<'a>(page: &'a Value, user_id: &UserId) -> impl Iterator<Item = &'a Value> {
	chunk(page).filter(move |event| {
		event["type"] == "m.room.member" && event["state_key"] == user_id.as_str()
	})
}

/// Whether a page carries the event that made the room world-readable.
///
/// The state before that event was not world-readable, yet a peek must see it.
fn opens_history(page: &Value) -> bool {
	chunk(page).any(|event| {
		event["type"] == "m.room.history_visibility"
			&& event["content"]["history_visibility"] == "world_readable"
	})
}

/// The events a page carries, in stream order.
///
/// A page whose chunk is absent, which the server sends for an empty one,
/// carries none.
fn chunk(page: &Value) -> impl Iterator<Item = &Value> {
	page["chunk"].as_array().into_iter().flatten()
}
