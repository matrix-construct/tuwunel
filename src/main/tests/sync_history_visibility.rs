#![cfg(test)]

use futures::{StreamExt, TryStreamExt, stream::iter};
use reqwest::Url;
use serde_json::{Value, json};
use tuwunel_core::{
	Result, implement,
	ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId, UserId, events::StateEventType},
};
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, register},
	fixture::boot,
	sync::joined_room,
};

mod client;
mod fixture;
mod sliding_sync;

#[expect(dead_code)] // Only the raw GET is used from the GET add-on.
#[path = "client/get.rs"]
mod get;

#[expect(dead_code)] // `admit` is unused from the membership add-on.
#[path = "client/membership.rs"]
mod membership;

#[path = "client/message.rs"]
mod message;

#[path = "client/put.rs"]
mod put;

#[path = "client/state.rs"]
mod state;

#[path = "client/sync.rs"]
mod sync;

const OWNER_TOKEN: &str = "sync-history-visibility-owner-access-token";
const MEMBER_TOKEN: &str = "sync-history-visibility-member-access-token";
const STRANGER_TOKEN: &str = "sync-history-visibility-stranger-access-token";

/// The hidden run exceeds the scan budget of a two-event window.
const HIDDEN_RUN: usize = 60;

struct Accounts<'a> {
	services: &'a Services,
	member_id: &'a UserId,
	owner: Client<'a>,
	member: Client<'a>,
	stranger: Client<'a>,
}

/// Sync timelines carry only the events the user may see.
///
/// A joiner's first window leaves out what history visibility hid before the
/// join, and a hidden state change ends the window so the state section covers
/// it. A window that stopped early still carries a token from which pagination
/// reaches the older events the user may see. A visibility read that fails
/// withholds the room from sliding sync until it reads again, while legacy
/// sync leaves the event out.
#[test]
fn sync_timelines_follow_history_visibility() -> Result {
	boot("sync-history-visibility", [], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "visibility-owner", OWNER_TOKEN).await?;
	register(services, "visibility-stranger", STRANGER_TOKEN).await?;

	let member_id = register(services, "visibility-member", MEMBER_TOKEN).await?;
	let accounts = Accounts {
		services,
		member_id: &member_id,
		owner: Client { services, base, token: OWNER_TOKEN },
		member: Client { services, base, token: MEMBER_TOKEN },
		stranger: Client { services, base, token: STRANGER_TOKEN },
	};

	accounts.pre_join_history().await?;
	accounts.hidden_state_ends_the_window().await?;
	accounts.peek_resumes_past_hidden_events().await?;
	accounts.hidden_increment_keeps_a_token().await?;

	accounts.scan_budget().await?;
	accounts.rejected_invite_keeps_its_leave().await?;
	accounts.failed_join_read_stays_in_state().await?;
	accounts
		.lookahead_failure_keeps_the_window()
		.await?;

	accounts.failed_visibility_read().await
}

/// A joiner's first window leaves out a message sent before the join under
/// `joined`, in legacy and sliding sync alike.
///
/// The message sent after the join proves the window is not simply empty.
#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn pre_join_history(&self) -> Result {
	let room_id = self.room("joined").await?;
	let before = self
		.owner
		.send_text(&room_id, "pre-join-before", "before join")
		.await?;

	self.member.act(&room_id, "join").await?;

	let after = self
		.owner
		.send_text(&room_id, "pre-join-after", "after join")
		.await?;

	let legacy = self.member.sync(None, 10).await?;
	let timeline = &joined_room(&legacy, &room_id)?["timeline"]["events"];

	assert!(!has_event(timeline, &before), "pre-join message in legacy sync: {legacy}");
	assert!(has_event(timeline, &after), "post-join message missing: {legacy}");

	let sliding = self
		.member
		.sliding_sync(None, 0, &list("pre-join", 10))
		.await?;

	let timeline = &sliding["rooms"][room_id.as_str()]["timeline"];

	assert!(!has_event(timeline, &before), "pre-join message in sliding sync: {sliding}");
	assert!(has_event(timeline, &after), "post-join message missing: {sliding}");

	Ok(())
}

/// A state change the joiner may not see ends its first window.
///
/// The window then starts at the join, the state section carries the change,
/// and pagination from the window's token reaches the older events the joiner
/// may see, past the hidden ones.
#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn hidden_state_ends_the_window(&self) -> Result {
	let room_id = self.room("joined").await?;
	let early = self
		.owner
		.send_text(&room_id, "boundary-early", "early")
		.await?;

	let topic = json!({ "topic": "before the join" });
	let topic = self
		.owner
		.put_state(&room_id, "m.room.topic", "", &topic)
		.await?;

	let late = self
		.owner
		.send_text(&room_id, "boundary-late", "late")
		.await?;

	self.member.act(&room_id, "join").await?;

	let join = self.member_event(&room_id).await?;
	let after = self
		.owner
		.send_text(&room_id, "boundary-after", "after join")
		.await?;

	let legacy = self.member.sync(None, 10).await?;
	let room = joined_room(&legacy, &room_id)?;
	let timeline = &room["timeline"];

	assert_eq!(event_ids(&timeline["events"]), [join.as_str(), after.as_str()], "{legacy}");
	assert_eq!(timeline["limited"], true, "window not limited: {legacy}");
	assert!(
		has_event(&room["state"]["events"], &topic),
		"hidden change not in state: {legacy}"
	);

	let history = self
		.member
		.messages_before(&room_id, field(timeline, "prev_batch")?)
		.await?;

	let chunk = &history["chunk"];

	assert!(has_type(chunk, "m.room.create"), "pagination stopped short: {history}");
	for hidden in [&early, &topic, &late] {
		assert!(!has_event(chunk, hidden), "hidden event paginated: {history}");
	}

	let sliding = self
		.member
		.sliding_sync(None, 0, &list("boundary", 10))
		.await?;

	let room = &sliding["rooms"][room_id.as_str()];

	assert_eq!(event_ids(&room["timeline"]), [join.as_str(), after.as_str()], "{sliding}");
	assert_eq!(room["limited"], true, "window not limited: {sliding}");

	Ok(())
}

/// A peek carries a token from which pagination reaches the public history
/// past a hidden event.
///
/// The window keeps the reopening, since the `world_readable` value it sets
/// satisfies the spec's before-or-after rule. The private message before it
/// stays hidden, both in the window and in pagination from its token.
#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn peek_resumes_past_hidden_events(&self) -> Result {
	let room_id = self.room("world_readable").await?;
	let public = self
		.owner
		.send_text(&room_id, "peek-public", "public")
		.await?;

	self.set_visibility(&room_id, "joined").await?;

	let private = self
		.owner
		.send_text(&room_id, "peek-private", "private")
		.await?;

	let reopened = self
		.set_visibility(&room_id, "world_readable")
		.await?;

	let subscription = json!({
		"conn_id": "peek",
		"room_subscriptions": {room_id.as_str(): {"timeline_limit": 1, "required_state": []}}
	});

	let sliding = self
		.stranger
		.sliding_sync(None, 0, &subscription)
		.await?;

	let room = &sliding["rooms"][room_id.as_str()];

	assert!(!has_event(&room["timeline"], &private), "hidden message peeked: {sliding}");
	assert!(
		has_event(&room["timeline"], &reopened),
		"reopening hidden from a peek: {sliding}"
	);

	let history = self
		.stranger
		.messages_before(&room_id, field(room, "prev_batch")?)
		.await?;

	assert!(has_event(&history["chunk"], &public), "public history unreachable: {history}");
	assert!(!has_event(&history["chunk"], &private), "hidden message paginated: {history}");

	Ok(())
}

/// An increment whose only event is left out still carries a token.
///
/// The member's room account data puts the room in the response while its one
/// new event, whose visibility cannot be read, is left out. The scan reaches
/// the client's position having kept nothing, so the window resumes pagination
/// below that event rather than offering no token.
#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn hidden_increment_keeps_a_token(&self) -> Result {
	let services = self.services;
	let room_id = self.room("shared").await?;

	self.member.act(&room_id, "join").await?;

	let since = self.member.sync(None, 10).await?;
	let since = field(&since, "next_batch")?;
	let message = self
		.owner
		.send_text(&room_id, "increment-unreadable", "unreadable")
		.await?;

	let shorteventid = services.short.get_shorteventid(&message).await?;
	let shortstatehash = services
		.state
		.pdu_shortstatehash(&message)
		.await?;

	let snapshots = &services.db["shorteventid_shortstatehash"];
	let marker =
		format!("user/{}/rooms/{room_id}/account_data/org.example.marker", self.member_id);

	self.member.put(&marker, &json!({})).await?;
	snapshots.insert(&shorteventid.to_be_bytes(), [0xFF_u8]);

	let reply = self.member.sync(Some(since), 10).await;

	snapshots.put(shorteventid, shortstatehash);

	let reply = reply?;
	let timeline = &joined_room(&reply, &room_id)?["timeline"];

	assert!(!has_event(&timeline["events"], &message), "unreadable event sent: {reply}");
	assert!(timeline["prev_batch"].is_string(), "increment carries no token: {reply}");

	Ok(())
}

/// A long run of hidden events ends the scan at its budget.
///
/// A two-event window judges at most fifty events, so it stops inside the run
/// before reaching the history it could see, and pagination from its token
/// still reaches that history.
#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn scan_budget(&self) -> Result {
	let room_id = self.room("world_readable").await?;
	let early = self
		.owner
		.send_text(&room_id, "budget-early", "early")
		.await?;

	let closed = self.set_visibility(&room_id, "joined").await?;

	iter(0..HIDDEN_RUN)
		.map(Ok)
		.try_for_each(async |index| {
			let txn_id = format!("budget-hidden-{index}");

			self.owner
				.send_text(&room_id, &txn_id, "hidden")
				.await
				.map(drop)
		})
		.await?;

	self.member.act(&room_id, "join").await?;

	let join = self.member_event(&room_id).await?;
	let legacy = self.member.sync(None, 2).await?;
	let timeline = &joined_room(&legacy, &room_id)?["timeline"];

	assert_eq!(event_ids(&timeline["events"]), [join.as_str()], "budget not applied: {legacy}");
	assert_eq!(timeline["limited"], true, "window not limited: {legacy}");

	let history = self
		.member
		.messages_before(&room_id, field(timeline, "prev_batch")?)
		.await?;

	assert!(has_event(&history["chunk"], &closed), "history unreachable: {history}");
	assert!(has_event(&history["chunk"], &early), "history unreachable: {history}");

	Ok(())
}

/// A rejected invite reaches the invitee's sync as their own leave, though
/// history visibility hides it, and nothing else the room hid comes with it.
///
/// The invite the leave answers is hidden state, so the window ends at the
/// leave, and the left room's state names no one but the invitee.
#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn rejected_invite_keeps_its_leave(&self) -> Result {
	let room_id = self.room("joined").await?;
	let since = self.member.sync(None, 10).await?;

	self.owner
		.act_on(&room_id, "invite", self.member_id)
		.await?;

	let before = self
		.owner
		.send_text(&room_id, "rejected-before", "while invited")
		.await?;

	self.member.act(&room_id, "leave").await?;

	let leave = self.member_event(&room_id).await?;
	let after = self
		.owner
		.send_text(&room_id, "rejected-after", "after leaving")
		.await?;

	let reply = self
		.member
		.sync(Some(field(&since, "next_batch")?), 10)
		.await?;

	let room = &reply["rooms"]["leave"][room_id.as_str()];
	let timeline = &room["timeline"]["events"];

	assert_eq!(event_ids(timeline), [leave.as_str()], "rejected invite timeline: {reply}");
	assert!(!has_event(timeline, &before), "hidden message sent: {reply}");
	assert!(!has_event(timeline, &after), "hidden message sent: {reply}");

	let others = room["state"]["events"]
		.as_array()
		.into_iter()
		.flatten()
		.any(|event| event["state_key"] != self.member_id.as_str());

	assert!(!others, "hidden room state sent: {reply}");

	Ok(())
}

/// A failed visibility read on the row past a full window does not withhold
/// the room.
///
/// The row past the window is judged only to learn that more events exist, so
/// a snapshot of its that cannot be read neither fails sliding sync nor shows.
#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn lookahead_failure_keeps_the_window(&self) -> Result {
	let services = self.services;
	let room_id = self.room("shared").await?;

	self.member.act(&room_id, "join").await?;

	let lookahead = self.set_visibility(&room_id, "joined").await?;
	let newest = self
		.owner
		.send_text(&room_id, "lookahead-newest", "newest")
		.await?;

	let shorteventid = services
		.short
		.get_shorteventid(&lookahead)
		.await?;

	let shortstatehash = services
		.state
		.pdu_shortstatehash(&lookahead)
		.await?;

	let snapshots = &services.db["shorteventid_shortstatehash"];

	snapshots.insert(&shorteventid.to_be_bytes(), [0xFF_u8]);

	let subscription = json!({
		"conn_id": "lookahead",
		"room_subscriptions": {room_id.as_str(): {"timeline_limit": 1, "required_state": []}}
	});

	let sliding = self
		.member
		.sliding_sync(None, 0, &subscription)
		.await?;

	snapshots.put(shorteventid, shortstatehash);

	let room = &sliding["rooms"][room_id.as_str()];

	assert_eq!(
		event_ids(&room["timeline"]),
		[newest.as_str()],
		"full window withheld: {sliding}"
	);
	assert_eq!(room["limited"], true, "window not limited: {sliding}");

	Ok(())
}

/// A join whose visibility cannot be read stays out of the joiner's legacy
/// timeline and in its state, in both state fields.
///
/// Legacy sync leaves the unreadable join out of the window, so the fallback
/// that moves a fresh join from state into an empty timeline must not bring it
/// back.
#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn failed_join_read_stays_in_state(&self) -> Result {
	let services = self.services;
	let room_id = self.room("shared").await?;
	let since = self.member.sync(None, 10).await?;
	let since = field(&since, "next_batch")?;

	self.member.act(&room_id, "join").await?;

	let join = self.member_event(&room_id).await?;
	let shorteventid = services.short.get_shorteventid(&join).await?;
	let shortstatehash = services.state.pdu_shortstatehash(&join).await?;
	let snapshots = &services.db["shorteventid_shortstatehash"];

	snapshots.insert(&shorteventid.to_be_bytes(), [0xFF_u8]);

	let legacy = self.member.sync(Some(since), 10).await;
	let after = ("org.matrix.msc4222.use_state_after", "true");
	let after = self
		.member
		.sync_with(Some(since), 10, Some(after))
		.await;

	snapshots.put(shorteventid, shortstatehash);

	let (legacy, after) = (legacy?, after?);
	let room = joined_room(&legacy, &room_id)?;

	assert!(!has_event(&room["timeline"]["events"], &join), "unreadable join sent: {legacy}");
	assert!(has_event(&room["state"]["events"], &join), "join left out of state: {legacy}");

	let room = joined_room(&after, &room_id)?;
	let state_after = &room["org.matrix.msc4222.state_after"]["events"];

	assert!(!has_event(&room["timeline"]["events"], &join), "unreadable join sent: {after}");
	assert!(has_event(state_after, &join), "join left out of state_after: {after}");

	Ok(())
}

/// A history visibility event that cannot be read withholds its room.
///
/// Sliding sync leaves the room out while a healthy room advances, and delivers
/// it exactly once at the next wake after the event reads again. Legacy sync
/// omits the event while the row is unreadable.
#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn failed_visibility_read(&self) -> Result {
	let quiet = self.room("shared").await?;
	let broken = self.room("shared").await?;
	let visibility = self.set_visibility(&broken, "shared").await?;

	self.member.act(&quiet, "join").await?;
	self.member.act(&broken, "join").await?;

	let list = list("failed-read", 10);
	let opened = self.member.sliding_sync(None, 0, &list).await?;
	let legacy = self.member.sync(None, 10).await?;
	let healthy = self
		.owner
		.send_text(&quiet, "failed-read-quiet", "quiet")
		.await?;

	let message = self
		.owner
		.send_text(&broken, "failed-read-broken", "broken")
		.await?;

	let stored = self.corrupt(&visibility).await?;
	let withheld = self
		.member
		.sliding_sync(Some(field(&opened, "pos")?), 0, &list)
		.await?;

	let rooms = &withheld["rooms"];

	assert!(has_event(&rooms[quiet.as_str()]["timeline"], &healthy), "{withheld}");
	assert!(rooms.get(broken.as_str()).is_none(), "unreadable room delivered: {withheld}");

	let omitted = self
		.member
		.sync(Some(field(&legacy, "next_batch")?), 10)
		.await?;

	let rooms = &omitted["rooms"]["join"];

	assert!(has_event(&rooms[quiet.as_str()]["timeline"]["events"], &healthy), "{omitted}");
	assert!(!has_event(&rooms[broken.as_str()]["timeline"]["events"], &message), "{omitted}");

	self.restore(&visibility, &stored).await?;
	self.owner
		.send_text(&quiet, "failed-read-wake", "wake")
		.await?;

	let retried = self
		.member
		.sliding_sync(Some(field(&withheld, "pos")?), 0, &list)
		.await?;

	let timeline = &retried["rooms"][broken.as_str()]["timeline"];

	assert!(has_event(timeline, &message), "withheld room not retried: {retried}");

	self.owner
		.send_text(&quiet, "failed-read-settle", "settle")
		.await?;

	let settled = self
		.member
		.sliding_sync(Some(field(&retried, "pos")?), 0, &list)
		.await?;

	let timeline = &settled["rooms"][broken.as_str()]["timeline"];

	assert!(!has_event(timeline, &message), "retried room delivered twice: {settled}");

	Ok(())
}

#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn room(&self, visibility: &str) -> Result<OwnedRoomId> {
	let history = json!({
		"type": "m.room.history_visibility",
		"content": { "history_visibility": visibility }
	});

	self.owner
		.create_room(&json!({ "preset": "public_chat", "initial_state": [history] }))
		.await
}

#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn set_visibility(&self, room_id: &RoomId, visibility: &str) -> Result<OwnedEventId> {
	let content = json!({ "history_visibility": visibility });

	self.owner
		.put_state(room_id, "m.room.history_visibility", "", &content)
		.await
}

#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn member_event(&self, room_id: &RoomId) -> Result<OwnedEventId> {
	self.services
		.state_accessor
		.room_state_get_id(room_id, &StateEventType::RoomMember, self.member_id.as_str())
		.await
}

/// Overwrites an event's stored row with bytes that do not parse.
///
/// Returns the event's JSON, re-encoded, for `restore` to write back.
#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn corrupt(&self, event_id: &EventId) -> Result<Vec<u8>> {
	let pdu: Value = self.services.timeline.get(event_id).await?;
	let pdu_id = self
		.services
		.timeline
		.get_pdu_id(event_id)
		.await?;

	self.services.db["pduid_pdu"].insert(pdu_id.as_ref(), b"{");

	Ok(serde_json::to_vec(&pdu)?)
}

#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn restore(&self, event_id: &EventId, stored: &[u8]) -> Result {
	let pdu_id = self
		.services
		.timeline
		.get_pdu_id(event_id)
		.await?;

	self.services.db["pduid_pdu"].insert(pdu_id.as_ref(), stored);

	Ok(())
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn sync(&self, since: Option<&str>, limit: u64) -> Result<Value> {
	self.sync_with(since, limit, None).await
}

/// One legacy sync with an extra query parameter, such as MSC4222's
/// `use_state_after`.
#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn sync_with(
	&self,
	since: Option<&str>,
	limit: u64,
	extra: Option<(&str, &str)>,
) -> Result<Value> {
	let filter = json!({ "room": { "timeline": { "limit": limit } } }).to_string();
	let query = [("timeout", "0"), ("filter", filter.as_str())]
		.into_iter()
		.chain(since.map(|since| ("since", since)))
		.chain(extra);

	let url = Url::parse_with_params(&self.url("sync"), query)?;
	let response = self.get_url(url.as_str()).await?;

	accepted(response).await
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn messages_before(&self, room_id: &RoomId, from: &str) -> Result<Value> {
	let path = format!("rooms/{room_id}/messages");
	let query = [("dir", "b"), ("from", from), ("limit", "100")];
	let url = Url::parse_with_params(&self.url(&path), query)?;
	let response = self.get_url(url.as_str()).await?;

	accepted(response).await
}

fn list(conn_id: &str, limit: u64) -> Value {
	json!({
		"conn_id": conn_id,
		"lists": {"all": {
			"ranges": [[0, 49]],
			"timeline_limit": limit,
			"required_state": []
		}}
	})
}

fn has_event(events: &Value, event_id: &EventId) -> bool {
	events
		.as_array()
		.into_iter()
		.flatten()
		.any(|event| event["event_id"] == event_id.as_str())
}

fn has_type(events: &Value, event_type: &str) -> bool {
	events
		.as_array()
		.into_iter()
		.flatten()
		.any(|event| event["type"] == event_type)
}

fn event_ids(events: &Value) -> Vec<&str> {
	events
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(|event| event["event_id"].as_str())
		.collect()
}
