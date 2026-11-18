//! What the sticky section serves each user: no erased sender's event reaches
//! a later joiner, and a sticky thread root bundles only a reply the user may
//! see.
//!
//! The section takes each event through the shaping a timeline event takes, so
//! these cases look for it only where a one-event timeline leaves it to the
//! section, in both sync versions.

use serde_json::{Value, json};
use tuwunel_core::{
	Result, err,
	ruma::{EventId, OwnedEventId, RoomId, UserId},
};
use tuwunel_service::Services;

use super::{
	DURATION, DURATION_MS, ONE_EVENT, STICKY_KEY,
	client::{Client, field, register},
	extension_events, extension_request, find, fresh_copy,
	require::require,
	sync::joined_room,
};

const ERIN: &str = "sticky-events-erin-token-000000000";
const FRANK: &str = "sticky-events-frank-token-00000000";
const GWEN: &str = "sticky-events-gwen-token-000000000";
const HANK: &str = "sticky-events-hank-token-000000000";

/// An erased sender's sticky event leaves the section of a user who joins
/// after the erasure, while a member joined at the event still receives it.
///
/// Frank's join fills both one-event timelines, so only the section could
/// carry Erin's event to him.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn erased_sender_leaves_the_section(
	services: &Services,
	base: &str,
	alice: &Client<'_>,
	room: &RoomId,
) -> Result {
	let erin_id = register(services, "erin", ERIN).await?;

	register(services, "frank", FRANK).await?;

	let erin = Client { services, base, token: ERIN };
	let frank = Client { services, base, token: FRANK };

	erin.act(room, "join").await?;

	let erased = send_sticky_text(&erin, room, "erased").await?;

	services
		.deactivate
		.full_deactivate(&erin_id, true)
		.await?;

	frank.act(room, "join").await?;

	let kept = alice
		.get(&format!("sync?timeout=0&filter={ONE_EVENT}"))
		.await?;

	require(
		find(&joined_room(&kept, room)?[STICKY_KEY]["events"], &erased).is_some_and(fresh_copy),
		"a member joined at the event lost an erased sender's sticky event",
	)?;

	for events in sections(&frank, room).await? {
		require(
			find(&events, &erased).is_none(),
			"a later joiner received an erased sender's sticky event",
		)?;
	}

	Ok(())
}

/// A sticky thread root reaching the section bundles no reply its user ignores
/// or may not see, and no participation of another user.
///
/// Bob ignores Gwen for this case alone, and Hank joins after her reply, which
/// the room's joined history visibility then hides from him. Alice sees the
/// reply bundled, as the thread's root sender.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn sticky_root_bundles_only_seen_replies(
	services: &Services,
	base: &str,
	alice: &Client<'_>,
	bob: &Client<'_>,
	bob_id: &UserId,
	room: &RoomId,
) -> Result {
	let gwen_id = register(services, "gwen", GWEN).await?;

	register(services, "hank", HANK).await?;

	let gwen = Client { services, base, token: GWEN };
	let hank = Client { services, base, token: HANK };

	gwen.act(room, "join").await?;

	let root = send_sticky_text(alice, room, "thread-root").await?;
	let reply = json!({
		"msgtype": "m.text",
		"body": "reply",
		"m.relates_to": { "rel_type": "m.thread", "event_id": root },
	});

	let reply = gwen
		.put(&format!("rooms/{room}/send/m.room.message/thread-reply"), &reply)
		.await?;

	let reply = field(&reply, "event_id")?;

	hank.act(room, "join").await?;

	for events in sections(alice, room).await? {
		require(
			latest_reply(&events, &root)?.0 == Some(reply),
			"the root sender's section lost the thread's latest reply",
		)?;
	}

	let ignored = format!("user/{bob_id}/account_data/m.ignored_user_list");

	bob.put(&ignored, &json!({ "ignored_users": { gwen_id.as_str(): {} } }))
		.await?;

	for viewer in [bob, &hank] {
		for events in sections(viewer, room).await? {
			let (latest, participated) = latest_reply(&events, &root)?;

			require(latest != Some(reply), "a section bundled a reply its user may not see")?;
			require(
				participated != Some(true),
				"a section bundled another user's participation",
			)?;
		}
	}

	bob.put(&ignored, &json!({ "ignored_users": {} }))
		.await
		.map(drop)
}

/// Sends a sticky message under its key as the transaction id.
///
/// Any member may send one, unlike the call membership the other cases send.
#[tracing::instrument(level = "debug", skip_all)]
async fn send_sticky_text(client: &Client<'_>, room: &RoomId, key: &str) -> Result<OwnedEventId> {
	let content = json!({ "msgtype": "m.text", "body": key, "msc4354_sticky_key": key });
	let path = format!("rooms/{room}/send/m.room.message/{key}?{DURATION}={DURATION_MS}");
	let response = client.put(&path, &content).await?;
	let event_id = field(&response, "event_id")?.try_into()?;

	Ok(event_id)
}

/// The legacy sync section and the sliding sync extension of a new sync.
///
/// Both timelines hold one event, so an older sticky event can arrive only in
/// the section.
#[tracing::instrument(level = "debug", skip_all)]
async fn sections(client: &Client<'_>, room: &RoomId) -> Result<[Value; 2]> {
	let sync = client
		.get(&format!("sync?timeout=0&filter={ONE_EVENT}"))
		.await?;

	let extension = client
		.sliding_sync(None, 0, &extension_request(&json!([])))
		.await?;

	let legacy = joined_room(&sync, room)?[STICKY_KEY]["events"].clone();

	Ok([legacy, extension_events(&extension, room).clone()])
}

/// The latest reply and the participation a section's copy of a thread root
/// bundles.
///
/// A section without the root fails the case, which would otherwise pass
/// vacuously.
fn latest_reply<'a>(
	events: &'a Value,
	root: &EventId,
) -> Result<(Option<&'a str>, Option<bool>)> {
	let root = find(events, root).ok_or_else(|| err!("a section omitted the sticky root"))?;
	let thread = &root["unsigned"]["m.relations"]["m.thread"];

	let latest = thread["latest_event"]["event_id"].as_str();
	let participated = thread["current_user_participated"].as_bool();

	Ok((latest, participated))
}
