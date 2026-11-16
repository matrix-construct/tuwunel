#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{
	Result, err, implement,
	ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId, UserId},
};
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, register},
	fixture::boot,
	sync::joined_room,
};

mod client;
mod fixture;

#[path = "client/event.rs"]
mod event;

#[path = "client/get.rs"]
mod get;

#[expect(dead_code)] // Only the self-membership action is used from the add-on.
#[path = "client/membership.rs"]
mod membership;

#[path = "client/put.rs"]
mod put;

#[path = "client/redact.rs"]
mod redact;

#[path = "client/room_event.rs"]
mod room_event;

#[path = "client/sync.rs"]
mod sync;

#[path = "client/thread.rs"]
mod thread;

const READER_TOKEN: &str = "bundle-requester-view-reader-token";
const IGNORED_TOKEN: &str = "bundle-requester-view-ignored-token";
const VISIBLE_TOKEN: &str = "bundle-requester-view-visible-token";
const LEAVER_TOKEN: &str = "bundle-requester-view-leaver-token";
const REJOINER_TOKEN: &str = "bundle-requester-view-rejoiner-token";

/// The summary search limit the budget roots are built around.
///
/// One budget root fits within it exactly and the other runs one row past it.
const SEARCH_LIMIT: &str = "rooms.relations.thread_summary_search_limit=4";

#[test]
fn requester_view_hides_ignored_hidden_and_redacted_relations() -> Result {
	boot("bundle-requester-view", [SEARCH_LIMIT], exercise)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	let reader_id = register(services, "viewreader", READER_TOKEN).await?;
	let ignored_id = register(services, "viewignored", IGNORED_TOKEN).await?;

	register(services, "viewvisible", VISIBLE_TOKEN).await?;
	let leaver_id = register(services, "viewleaver", LEAVER_TOKEN).await?;

	register(services, "viewrejoiner", REJOINER_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let ignored = Client { services, base, token: IGNORED_TOKEN };
	let visible = Client { services, base, token: VISIBLE_TOKEN };
	let leaver = Client { services, base, token: LEAVER_TOKEN };
	let rejoiner = Client { services, base, token: REJOINER_TOKEN };

	reader
		.set_ignored(&reader_id, &ignored_id)
		.await?;

	relations_view(&reader, &ignored, &visible).await?;
	thread_views(&reader, &reader_id, &ignored, &visible).await?;
	former_member_view(&reader, &leaver, &leaver_id, &visible).await?;
	rejoiner_view(&rejoiner, &visible).await
}

/// MSC2675: `/relations` hides children from an ignored sender, and redacted
/// children from every requester.
///
/// Recursion inherits both filters, a hidden child at `to` still ends the page,
/// and an ignored user's state event stays, as does every unredacted child for
/// a requester who ignores nobody.
#[tracing::instrument(level = "debug", skip_all)]
async fn relations_view(
	reader: &Client<'_>,
	ignored: &Client<'_>,
	visible: &Client<'_>,
) -> Result {
	// Members may send state, so the ignored user can relate a state event.
	let open_state = json!({
		"preset": "public_chat",
		"power_level_content_override": { "state_default": 0 },
	});

	let room = visible.create_room(&open_state).await?;

	reader.act(&room, "join").await?;
	ignored.act(&room, "join").await?;

	let root = visible
		.send_text(&room, "relations-root", "relations root")
		.await?;

	let shown = visible
		.react(&room, "relations-shown", &root, "a")
		.await?;

	let ignored_reaction = ignored
		.react(&room, "relations-ignored", &root, "a")
		.await?;

	let ignored_edit = ignored
		.replace(&room, "relations-ignored-edit", &root)
		.await?;

	let reply = visible
		.reply(&room, "relations-reply", &root)
		.await?;

	let redacted = visible
		.react(&room, "relations-redacted", &root, "b")
		.await?;

	// Positions are read before the redaction hides the reaction from every listing.
	let listed = visible.relations(&room, &root, "?dir=f").await?;
	let redacted_at = field(&listed, "next_batch")?.to_owned();
	let first_two = visible
		.relations(&room, &root, "?dir=f&limit=2")
		.await?;

	let ignored_at = field(&first_two, "next_batch")?.to_owned();

	visible
		.redact(&room, &redacted, "relations-redaction")
		.await?;

	let later = visible
		.react(&room, "relations-later", &root, "c")
		.await?;

	let relating = json!({ "m.relates_to": { "rel_type": "m.reference", "event_id": root } });
	let ignored_state = ignored
		.put_event(&format!("rooms/{room}/state/org.example.relating/relating"), &relating)
		.await?;

	let ignored_nested = ignored
		.react(&room, "relations-ignored-nested", &reply, "a")
		.await?;

	let shown_nested = visible
		.react(&room, "relations-shown-nested", &reply, "a")
		.await?;

	let all = reader.relations(&room, &root, "").await?;
	let all = chunk_ids(&all)?;

	assert!(all.contains(&shown.as_str()), "a visible reaction is missing");
	assert!(all.contains(&reply.as_str()), "a visible thread reply is missing");
	assert!(
		all.contains(&ignored_state.as_str()),
		"an ignored user's state event is missing"
	);
	assert!(!all.contains(&ignored_reaction.as_str()), "an ignored reaction was served");
	assert!(!all.contains(&ignored_edit.as_str()), "an ignored edit was served");
	assert!(!all.contains(&redacted.as_str()), "a redacted reaction was served");

	let annotations = reader
		.relations(&room, &root, "/m.annotation")
		.await?;

	let annotations = chunk_ids(&annotations)?;

	assert_eq!(annotations, [later.as_str(), shown.as_str()], "annotations kept a hidden one");

	let recursed = reader
		.relations(&room, &root, "?recurse=true")
		.await?;

	let recursed = chunk_ids(&recursed)?;

	assert!(recursed.contains(&shown_nested.as_str()), "recursion lost a visible reaction");
	assert!(
		!recursed.contains(&ignored_nested.as_str()),
		"recursion served an ignored reaction",
	);

	assert!(!recursed.contains(&ignored_edit.as_str()), "recursion served an ignored edit");
	assert!(!recursed.contains(&redacted.as_str()), "recursion served a redacted reaction");

	let to_ignored = reader
		.relations(&room, &root, &format!("?dir=f&to={ignored_at}"))
		.await?;

	assert_eq!(chunk_ids(&to_ignored)?, [shown.as_str()], "a hidden `to` did not end the page");

	let to_redacted = reader
		.relations(&room, &root, &format!("?dir=f&to={redacted_at}"))
		.await?;

	assert_eq!(
		chunk_ids(&to_redacted)?,
		[shown.as_str(), reply.as_str()],
		"a redacted `to` did not end the page",
	);

	let control = visible.relations(&room, &root, "").await?;
	let control = chunk_ids(&control)?;

	assert!(
		control.contains(&ignored_reaction.as_str()) && control.contains(&ignored_edit.as_str()),
		"a requester ignoring nobody lost another user's relations",
	);

	assert!(!control.contains(&redacted.as_str()), "a redacted reaction was served");

	Ok(())
}

/// MSC3440: every folded path serves a requester who ignores the latest
/// replier the previous reply, and a count without the ignored replies.
///
/// A summary with no reply left to show is omitted, `/relations` agrees with
/// the count, and the stored summary other members see stays whole.
#[tracing::instrument(level = "debug", skip_all)]
async fn thread_views(
	reader: &Client<'_>,
	reader_id: &UserId,
	ignored: &Client<'_>,
	visible: &Client<'_>,
) -> Result {
	let room = joined_room_of(reader, ignored, visible).await?;
	let unshown = visible
		.send_text(&room, "views-unshown", "unshown root")
		.await?;

	ignored
		.reply(&room, "views-unshown-reply", &unshown)
		.await?;

	let root = visible
		.send_text(&room, "views-root", "views root")
		.await?;

	let shown = visible.reply(&room, "views-shown", &root).await?;

	let hidden = ignored
		.reply(&room, "views-ignored", &root)
		.await?;

	assert_summary(&reader.event(&room, &root).await?, &shown, 1, "/event");

	let messages = reader
		.get(&format!("rooms/{room}/messages?dir=b&limit=10"))
		.await?;

	assert_summary(find(&messages["chunk"], &root)?, &shown, 1, "/messages");

	let context = reader
		.get(&format!("rooms/{room}/context/{root}?limit=2"))
		.await?;

	assert_summary(&context["event"], &shown, 1, "/context");

	let filter = json!({ "room": { "timeline": { "limit": 3 } } });
	let sync = reader.sync_filtered(reader_id, &filter).await?;

	let timeline = &joined_room(&sync, &room)?["timeline"];

	assert_eq!(timeline["limited"], true, "the sync timeline was not limited");
	assert_summary(find(&timeline["events"], &root)?, &shown, 1, "/sync");

	let threads = reader.threads(&room, 10, None).await?;

	assert_summary(find(&threads["chunk"], &root)?, &shown, 1, "/threads");

	let replies = reader
		.relations(&room, &root, "/m.thread")
		.await?;

	assert_eq!(chunk_ids(&replies)?, [shown.as_str()], "/relations disagrees with count");

	assert_summary(&visible.event(&room, &root).await?, &hidden, 2, "stored /event");

	let unshown = reader.event(&room, &unshown).await?;

	assert!(thread_summary(&unshown).is_none(), "a summary was served without its latest");

	search_budget(reader, ignored, visible, &room).await
}

/// The summary walk reads at most the search limit's relation rows.
///
/// A reply the requester may see on the last row still serves the summary, and
/// one row past the limit omits it.
#[tracing::instrument(level = "debug", skip_all)]
async fn search_budget(
	reader: &Client<'_>,
	ignored: &Client<'_>,
	visible: &Client<'_>,
	room: &RoomId,
) -> Result {
	let at_limit = visible
		.send_text(room, "budget-at", "budget root at the limit")
		.await?;

	let past_limit = visible
		.send_text(room, "budget-past", "budget root past the limit")
		.await?;

	let reached = visible
		.reply(room, "budget-at-shown", &at_limit)
		.await?;

	visible
		.reply(room, "budget-past-shown", &past_limit)
		.await?;

	for index in 0..4 {
		if index < 3 {
			ignored
				.reply(room, &format!("budget-at-ignored-{index}"), &at_limit)
				.await?;
		}

		ignored
			.reply(room, &format!("budget-past-ignored-{index}"), &past_limit)
			.await?;
	}

	assert_summary(&reader.event(room, &at_limit).await?, &reached, 1, "/event at the limit");

	let past_limit = reader.event(room, &past_limit).await?;

	assert!(thread_summary(&past_limit).is_none(), "the walk read past its limit");

	Ok(())
}

/// History visibility: a former member is shown no reply sent after they left.
///
/// The summary keeps the replies from before the leave, on `/event` and in the
/// left room's sync, while a current member sees every reply.
#[tracing::instrument(level = "debug", skip_all)]
async fn former_member_view(
	reader: &Client<'_>,
	leaver: &Client<'_>,
	leaver_id: &UserId,
	visible: &Client<'_>,
) -> Result {
	let room = visible
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	reader.act(&room, "join").await?;
	leaver.act(&room, "join").await?;

	let kept = visible
		.send_text(&room, "former-kept", "root kept by the leaver")
		.await?;

	let later = visible
		.send_text(&room, "former-later", "root replied after the leave")
		.await?;

	let before = visible
		.reply(&room, "former-before", &kept)
		.await?;

	leaver.act(&room, "leave").await?;

	let after = visible
		.reply(&room, "former-after", &kept)
		.await?;

	visible
		.reply(&room, "former-later-reply", &later)
		.await?;

	assert_summary(&leaver.event(&room, &kept).await?, &before, 1, "former /event");

	let filter = json!({ "room": { "include_leave": true } });
	let sync = leaver.sync_filtered(leaver_id, &filter).await?;

	let left = &sync["rooms"]["leave"][room.as_str()]["timeline"]["events"];

	assert_summary(find(left, &kept)?, &before, 1, "former /sync");

	let later = leaver.event(&room, &later).await?;

	assert!(thread_summary(&later).is_none(), "a former member was shown a later reply");
	assert_summary(&reader.event(&room, &kept).await?, &after, 2, "member /event");

	Ok(())
}

/// History visibility `joined`: a returning member misses the away replies.
///
/// Being joined again does not show them the replies sent while they were
/// away, while the sender still sees the whole thread.
#[tracing::instrument(level = "debug", skip_all)]
async fn rejoiner_view(rejoiner: &Client<'_>, visible: &Client<'_>) -> Result {
	let visibility = json!({
		"type": "m.room.history_visibility",
		"state_key": "",
		"content": { "history_visibility": "joined" },
	});

	let room = visible
		.create_room(&json!({ "preset": "public_chat", "initial_state": [visibility] }))
		.await?;

	rejoiner.act(&room, "join").await?;

	let root = visible
		.send_text(&room, "rejoin-root", "rejoin root")
		.await?;

	let before = visible
		.reply(&room, "rejoin-before", &root)
		.await?;

	rejoiner.act(&room, "leave").await?;

	let away = visible.reply(&room, "rejoin-away", &root).await?;

	rejoiner.act(&room, "join").await?;

	assert_summary(&rejoiner.event(&room, &root).await?, &before, 1, "rejoiner /event");
	assert_summary(&visible.event(&room, &root).await?, &away, 2, "sender /event");

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn joined_room_of(
	reader: &Client<'_>,
	ignored: &Client<'_>,
	visible: &Client<'_>,
) -> Result<OwnedRoomId> {
	let room = visible
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	reader.act(&room, "join").await?;
	ignored.act(&room, "join").await?;

	Ok(room)
}

#[track_caller]
fn assert_summary(event: &Value, latest: &EventId, count: u64, path: &str) {
	let summary = thread_summary(event).unwrap_or_else(|| panic!("{path}: no thread summary"));

	assert_eq!(summary["latest_event"]["event_id"], latest.as_str(), "{path}: latest_event");
	assert_eq!(summary["count"], count, "{path}: count");
}

fn thread_summary(event: &Value) -> Option<&Value> {
	event.pointer("/unsigned/m.relations/m.thread")
}

fn find<'a>(events: &'a Value, event_id: &EventId) -> Result<&'a Value> {
	events
		.as_array()
		.into_iter()
		.flatten()
		.find(|event| event["event_id"] == event_id.as_str())
		.ok_or_else(|| err!("response omitted {event_id}"))
}

fn chunk_ids(response: &Value) -> Result<Vec<&str>> {
	response["chunk"]
		.as_array()
		.ok_or_else(|| err!("relations response omitted its chunk"))?
		.iter()
		.map(|event| {
			event["event_id"]
				.as_str()
				.ok_or_else(|| err!("related event omitted its id"))
		})
		.collect()
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn sync_filtered(&self, user_id: &UserId, filter: &Value) -> Result<Value> {
	let filter = self
		.post(&format!("user/{user_id}/filter"), filter)
		.await?;

	self.get(&format!("sync?filter={}", field(&filter, "filter_id")?))
		.await
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn relations(&self, room_id: &RoomId, event_id: &EventId, rest: &str) -> Result<Value> {
	let path = format!("rooms/{room_id}/relations/{event_id}{rest}");
	let response = self
		.get_url(&self.client_url("v1/", &path))
		.await?;

	accepted(response).await
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn react(
	&self,
	room_id: &RoomId,
	txn_id: &str,
	target: &EventId,
	key: &str,
) -> Result<OwnedEventId> {
	let content = json!({
		"m.relates_to": { "rel_type": "m.annotation", "event_id": target, "key": key },
	});

	let path = format!("rooms/{room_id}/send/m.reaction/{txn_id}");

	self.put_event(&path, &content).await
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn replace(
	&self,
	room_id: &RoomId,
	txn_id: &str,
	target: &EventId,
) -> Result<OwnedEventId> {
	let content = json!({
		"msgtype": "m.text",
		"body": "* replaced",
		"m.new_content": { "msgtype": "m.text", "body": "replaced" },
		"m.relates_to": { "rel_type": "m.replace", "event_id": target },
	});

	let path = format!("rooms/{room_id}/send/m.room.message/{txn_id}");

	self.put_event(&path, &content).await
}
