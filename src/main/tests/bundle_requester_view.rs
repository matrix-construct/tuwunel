#![cfg(test)]

use std::array::from_fn;

use futures::{StreamExt, stream::try_unfold};
use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{
	Result, err, implement,
	ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId, UserId},
	utils::stream::TryReadyExt,
};
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, register},
	fixture::boot,
	relations_parent::{membership_visibility, requester_visibility, unfolded_view},
	sync::joined_room,
};

mod client;
mod fixture;

#[path = "client/event.rs"]
mod event;

#[path = "client/get.rs"]
mod get;

#[expect(dead_code)] // The add-on's invite-and-join helper is unused here.
#[path = "client/membership.rs"]
mod membership;

#[path = "client/put.rs"]
mod put;

#[path = "client/redact.rs"]
mod redact;

#[path = "bundle_requester_view/relations_parent.rs"]
mod relations_parent;

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
const ERASED_TOKEN: &str = "bundle-requester-view-erased-token";

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
	let erased_id = register(services, "viewerased", ERASED_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let ignored = Client { services, base, token: IGNORED_TOKEN };
	let visible = Client { services, base, token: VISIBLE_TOKEN };
	let leaver = Client { services, base, token: LEAVER_TOKEN };
	let rejoiner = Client { services, base, token: REJOINER_TOKEN };
	let erased = Client { services, base, token: ERASED_TOKEN };

	reader
		.set_ignored(&reader_id, &ignored_id)
		.await?;

	relations_view(&reader, &ignored, &visible).await?;
	relations_recursion(&visible).await?;
	requester_visibility(&reader, &ignored, &visible, &leaver).await?;
	membership_visibility(&visible, &leaver, &leaver_id).await?;
	unfolded_view(&visible, &erased, &erased_id, &leaver).await?;
	thread_views(&reader, &reader_id, &ignored, &visible).await?;
	former_member_view(&reader, &leaver, &leaver_id, &visible).await?;
	thread_roots(&visible).await?;
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
	let listed = visible
		.relations(&room, &root, "?dir=b&limit=1")
		.await?;
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

/// MSC3981: recursion respects filters, event order, paging, and its depth bound.
///
/// Filters stop the walk; both directions preserve event order and page without
/// loss. The recursion depth bound is reported even when the chunk is empty.
#[tracing::instrument(level = "debug", skip_all)]
async fn relations_recursion(visible: &Client<'_>) -> Result {
	let room = visible
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let root = visible
		.send_text(&room, "recursion-root", "recursion root")
		.await?;

	let thread_a1 = visible
		.reply(&room, "recursion-a1", &root)
		.await?;

	let reaction_b = visible
		.react(&room, "recursion-b", &thread_a1, "b")
		.await?;

	let thread_a2 = visible
		.reply(&room, "recursion-a2", &root)
		.await?;

	let ref_f = reference(visible, &room, "recursion-f", &root).await?;
	let ref_g = reference(visible, &room, "recursion-g", &ref_f).await?;
	let ref_e = reference(visible, &room, "recursion-e", &thread_a1).await?;
	let ref_h = reference(visible, &room, "recursion-h", &ref_g).await?;

	reference(visible, &room, "recursion-i", &ref_h).await?;

	let content = json!({
		"m.relates_to": { "rel_type": "m.reference", "event_id": root },
	});

	let custom_k = visible
		.put_event(&format!("rooms/{room}/send/org.example.reference/recursion-k"), &content)
		.await?;

	let ref_j = reference(visible, &room, "recursion-j", &custom_k).await?;
	let filtered = visible
		.relations(&room, &root, "/m.reference?recurse=true&dir=f")
		.await?;

	assert_eq!(
		chunk_ids(&filtered)?,
		[&ref_f, &ref_g, &ref_h, &custom_k, &ref_j].map(|id| id.as_str()),
		"reference recursion crossed filtered thread A1 to E or violated the three-level depth \
		 bound",
	);

	let forward = [
		&thread_a1,
		&reaction_b,
		&thread_a2,
		&ref_f,
		&ref_g,
		&ref_e,
		&ref_h,
		&custom_k,
		&ref_j,
	]
	.map(|id| id.as_str());

	let backward = from_fn::<_, 9, _>(|index| forward[8_usize.saturating_sub(index)]);

	for (dir, expected) in [("f", forward), ("b", backward)] {
		let response = visible
			.relations(&room, &root, &format!("?recurse=true&dir={dir}"))
			.await?;

		assert_eq!(
			chunk_ids(&response)?,
			expected,
			"recursive event order regressed in dir={dir}"
		);

		let paged = recursion_pages(visible, &room, &root, dir, &expected).await?;

		assert_eq!(paged, expected.len(), "recursive paging lost events in dir={dir}");
	}

	let recursed = visible
		.relations(&room, &root, "?recurse=true")
		.await?;

	assert_eq!(
		recursed.get("recursion_depth"),
		Some(&json!(3)),
		"root recursion depth regressed"
	);

	let empty = visible
		.relations(&room, &thread_a2, "?recurse=true")
		.await?;

	assert!(chunk_ids(&empty)?.is_empty(), "a childless reply gained recursive relations");
	assert_eq!(
		empty.get("recursion_depth"),
		Some(&json!(3)),
		"empty recursion lost its depth bound"
	);

	let direct = visible.relations(&room, &root, "").await?;

	assert!(
		direct.get("recursion_depth").is_none(),
		"non-recursive relations gained recursion_depth"
	);

	let messages = visible
		.relations(&room, &root, "/m.reference/m.room.message?recurse=true&dir=f")
		.await?;

	assert_eq!(
		chunk_ids(&messages)?,
		[ref_f.as_str(), ref_g.as_str(), ref_h.as_str()],
		"event-type recursion crossed K to J, a filtered thread, or the depth bound",
	);

	let first = visible
		.relations(&room, &root, "?dir=f&limit=1")
		.await?;

	let token = field(&first, "next_batch")?;
	let bounded = visible
		.relations(&room, &root, &format!("?recurse=true&dir=b&to={token}"))
		.await?;

	assert_eq!(
		chunk_ids(&bounded)?,
		&backward[..8],
		"recursive to boundary lost A1's children B and E or included A1",
	);

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn reference(
	client: &Client<'_>,
	room: &RoomId,
	txn: &str,
	target: &EventId,
) -> Result<OwnedEventId> {
	let content = json!({
		"msgtype": "m.text",
		"body": "reference",
		"m.relates_to": { "rel_type": "m.reference", "event_id": target },
	});

	let path = format!("rooms/{room}/send/m.room.message/{txn}");

	client.put_event(&path, &content).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn recursion_pages(
	client: &Client<'_>,
	room: &RoomId,
	root: &EventId,
	dir: &str,
	expected: &[&str],
) -> Result<usize> {
	let query = format!("?recurse=true&dir={dir}&limit=1");

	try_unfold(Some(query.clone()), async |next| {
		let Some(next) = next else { return Ok(None) };
		let response = client.relations(room, root, &next).await?;
		let chunk = response["chunk"]
			.as_array()
			.ok_or_else(|| err!("relations response omitted its chunk"))?;

		let next = response["next_batch"]
			.as_str()
			.filter(|_| !chunk.is_empty())
			.map(|token| format!("{query}&from={token}"));

		Ok(Some((response, next)))
	})
	.take(11)
	.ready_try_fold(0_usize, |offset, response| {
		let chunk = response["chunk"]
			.as_array()
			.ok_or_else(|| err!("relations response omitted its chunk"))?;

		let end = offset.saturating_add(chunk.len());

		assert!(
			expected.get(offset..end).is_some_and(|expected| {
				chunk
					.iter()
					.map(|event| event["event_id"].as_str())
					.eq(expected.iter().copied().map(Some))
			}),
			"recursive paging lost or reordered events in dir={dir}: {response}",
		);

		Ok(end)
	})
	.await
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

	let event = reader.event(&room, &root).await?;

	assert_summary(&event, &shown, 1, "/event");
	assert_latest_room(&event, Some(&*room), "/event");

	let messages = reader
		.get(&format!("rooms/{room}/messages?dir=b&limit=10"))
		.await?;

	let listed = find(&messages["chunk"], &root)?;

	assert_summary(listed, &shown, 1, "/messages");
	assert_latest_room(listed, Some(&*room), "/messages");

	let context = reader
		.get(&format!("rooms/{room}/context/{root}?limit=2"))
		.await?;

	assert_summary(&context["event"], &shown, 1, "/context");
	assert_latest_room(&context["event"], Some(&*room), "/context");

	let filter = json!({ "room": { "timeline": { "limit": 3 } } });
	let sync = reader.sync_filtered(reader_id, &filter).await?;

	let timeline = &joined_room(&sync, &room)?["timeline"];

	assert_eq!(timeline["limited"], true, "the sync timeline was not limited");

	let synced = find(&timeline["events"], &root)?;

	assert_summary(synced, &shown, 1, "/sync");
	assert_latest_room(synced, None, "/sync");

	let threads = reader.threads(&room, 10, None).await?;
	let listed = find(&threads["chunk"], &root)?;

	assert_summary(listed, &shown, 1, "/threads");
	assert_latest_room(listed, Some(&*room), "/threads");

	let replies = reader
		.relations(&room, &root, "/m.thread")
		.await?;

	assert_eq!(chunk_ids(&replies)?, [shown.as_str()], "/relations disagrees with count");

	assert_summary(&visible.event(&room, &root).await?, &hidden, 2, "stored /event");

	let unshown = reader.event(&room, &unshown).await?;

	assert!(thread_summary(&unshown).is_none(), "a summary was served without its latest");

	own_latest_view(reader, ignored, visible, &room, &root).await?;
	search_budget(reader, ignored, visible, &room).await
}

/// A requester's own latest reply carries its transaction ID, which nobody else
/// is served.
///
/// That holds for the stored latest reply and for one the requester's view
/// swaps in past an ignored reply.
#[tracing::instrument(level = "debug", skip_all)]
async fn own_latest_view(
	reader: &Client<'_>,
	ignored: &Client<'_>,
	visible: &Client<'_>,
	room: &RoomId,
	root: &EventId,
) -> Result {
	let own = ignored.event(room, root).await?;

	assert_eq!(latest_transaction_id(&own), Some("views-ignored"), "the sender's own latest");

	let other = visible.event(room, root).await?;

	assert_eq!(latest_transaction_id(&other), None, "another member's view of it");

	let swapped_root = visible
		.send_text(room, "views-own-root", "own latest root")
		.await?;

	let mine = reader
		.reply(room, "views-own", &swapped_root)
		.await?;

	ignored
		.reply(room, "views-own-ignored", &swapped_root)
		.await?;

	let swapped = reader.event(room, &swapped_root).await?;

	assert_summary(&swapped, &mine, 1, "swapped /event");
	assert_eq!(latest_transaction_id(&swapped), Some("views-own"), "the swapped-in own latest");

	Ok(())
}

fn latest_transaction_id(event: &Value) -> Option<&str> {
	event
		.pointer("/unsigned/m.relations/m.thread/latest_event/unsigned/transaction_id")
		.and_then(Value::as_str)
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
/// The former member sees only the replies from before the leave, on `/event`,
/// in the left room's sync and on `/relations` for that root, while a current
/// member sees every reply. Once the former member forgets the room,
/// `/relations` refuses that root as not found.
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

	let relations = leaver.relations(&room, &kept, "").await?;
	let relations = chunk_ids(&relations)?;

	assert!(relations.contains(&before.as_str()), "former /relations lost an earlier reply");
	assert!(!relations.contains(&after.as_str()), "former /relations served a later reply");

	let filter = json!({ "room": { "include_leave": true } });
	let sync = leaver.sync_filtered(leaver_id, &filter).await?;

	let left = &sync["rooms"]["leave"][room.as_str()]["timeline"]["events"];

	assert_summary(find(left, &kept)?, &before, 1, "former /sync");

	let later = leaver.event(&room, &later).await?;

	assert!(thread_summary(&later).is_none(), "a former member was shown a later reply");
	assert_summary(&reader.event(&room, &kept).await?, &after, 2, "member /event");

	leaver.act(&room, "forget").await?;

	let forgotten = leaver.try_relations(&room, &kept, "").await?;

	refused(forgotten, StatusCode::NOT_FOUND, "M_NOT_FOUND").await?;

	Ok(())
}

/// MSC3440: a thread may not root on an event that is itself a relation's child.
///
/// A send naming an edit or a caption fallback as its root is refused, while a
/// plain reply roots a thread, and a state event takes a reply but no summary.
#[tracing::instrument(level = "debug", skip_all)]
async fn thread_roots(visible: &Client<'_>) -> Result {
	let room = visible
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let send = |txn_id| format!("rooms/{room}/send/m.room.message/{txn_id}");
	let message = visible
		.send_text(&room, "roots-message", "roots message")
		.await?;

	let edit = visible
		.replace(&room, "roots-edit", &message)
		.await?;

	let caption = json!({
		"msgtype": "m.text",
		"body": "caption",
		"m.relates_to": {
			"rel_type": "m.caption_fallback",
			"event_id": message,
			"m.in_reply_to": { "event_id": message },
		},
	});

	let caption = visible
		.put_event(&send("roots-caption"), &caption)
		.await?;

	let plain = json!({
		"msgtype": "m.text",
		"body": "plain reply",
		"m.relates_to": { "m.in_reply_to": { "event_id": message } },
	});

	let plain = visible
		.put_event(&send("roots-plain"), &plain)
		.await?;

	let topic = visible
		.put_event(&format!("rooms/{room}/state/m.room.topic"), &json!({ "topic": "roots" }))
		.await?;

	for (root, txn_id) in [(&edit, "roots-on-edit"), (&caption, "roots-on-caption")] {
		let response = visible.try_thread(&room, txn_id, root).await?;

		refused(response, StatusCode::BAD_REQUEST, "M_UNKNOWN").await?;
	}

	let reply = visible
		.reply(&room, "roots-on-plain", &plain)
		.await?;

	assert_summary(&visible.event(&room, &plain).await?, &reply, 1, "plain reply root");

	visible
		.reply(&room, "roots-on-topic", &topic)
		.await?;

	let topic = visible.event(&room, &topic).await?;

	assert!(thread_summary(&topic).is_none(), "a state event was given a thread summary");

	Ok(())
}

/// Start a thread on `root` as this user and keep the raw reply.
///
/// The status is the caller's to judge, so a refused root can be asserted.
#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn try_thread(&self, room_id: &RoomId, txn_id: &str, root: &EventId) -> Result<Response> {
	let content = json!({
		"msgtype": "m.text",
		"body": "nested thread",
		"m.relates_to": { "rel_type": "m.thread", "event_id": root },
	});

	let path = format!("rooms/{room_id}/send/m.room.message/{txn_id}");

	self.request(Method::PUT, &self.url(&path), &content)
		.await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn refused(response: Response, status: StatusCode, errcode: &str) -> Result<Value> {
	let actual = response.status();
	let body: Value = response.json().await?;

	assert_eq!(actual, status, "{body}");
	assert_eq!(body["errcode"], errcode, "{body}");

	Ok(body)
}

/// History visibility `joined`: a returning member misses the away replies.
///
/// Being joined again does not show them the replies sent while they were
/// away, while the sender still sees the whole thread.
#[tracing::instrument(level = "debug", skip_all)]
async fn rejoiner_view(rejoiner: &Client<'_>, visible: &Client<'_>) -> Result {
	let room = room_with_history(visible, "joined").await?;

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
async fn room_with_history(
	creator: &Client<'_>,
	history_visibility: &str,
) -> Result<OwnedRoomId> {
	let visibility = json!({
		"type": "m.room.history_visibility",
		"state_key": "",
		"content": { "history_visibility": history_visibility },
	});

	creator
		.create_room(&json!({ "preset": "public_chat", "initial_state": [visibility] }))
		.await
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

/// Asserts the room the bundled latest event names, if any.
///
/// The latest event names its room where the event bundling it does: in the
/// room formats, and never in sync (MSC3440).
#[track_caller]
fn assert_latest_room(event: &Value, room: Option<&RoomId>, path: &str) {
	let room_id = event.pointer("/unsigned/m.relations/m.thread/latest_event/room_id");

	assert_eq!(room_id, room.map(|room| json!(room)).as_ref(), "{path}: latest room");
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
	let response = self
		.try_relations(room_id, event_id, rest)
		.await?;

	accepted(response).await
}

/// Get an event's relations as this user and keep the raw reply.
///
/// The status is the caller's to judge, so a refused parent can be asserted.
#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn try_relations(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	rest: &str,
) -> Result<Response> {
	let path = format!("rooms/{room_id}/relations/{event_id}{rest}");

	self.get_url(&self.client_url("v1/", &path)).await
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
