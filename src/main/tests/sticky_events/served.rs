//! What the sticky section serves each user: no erased sender's event reaches
//! a later joiner, a sticky thread root bundles only a reply the user may see,
//! and no sender on a forbidden server is served.
//!
//! The section takes each event through the shaping a timeline event takes, so
//! these cases look for it only where a one-event timeline leaves it to the
//! section, in both sync versions.

use std::{
	fs::write,
	iter::{empty, once},
};

use serde_json::{Value, json};
use tuwunel_core::{
	Result, err,
	ruma::{
		EventId, OwnedEventId, RoomId, UserId,
		events::room::member::{MembershipState, RoomMemberEventContent},
	},
};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::Services;

use super::{
	DURATION, DURATION_MS, ONE_EVENT, STICKY_KEY,
	client::{Client, field, register},
	extension_events, extension_request, find, fresh_copy,
	require::require,
	sticky_text,
	sync::joined_room,
};

const ERIN: &str = "sticky-events-erin-token-000000000";
const FRANK: &str = "sticky-events-frank-token-00000000";
const GWEN: &str = "sticky-events-gwen-token-000000000";
const HANK: &str = "sticky-events-hank-token-000000000";
const REMOTE: &str = "remote.invalid";
const FORBIDDEN: &str = "forbidden.invalid";

const DENY_LIST: &str =
	"[global.federation]\nforbidden_remote_server_names = ['^forbidden\\.invalid$']\n";

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

/// A sticky event whose sender's server a configuration reload forbids leaves
/// both sections, while one from another remote server stays.
///
/// Both remote users join and send through the local build, and Alice's message
/// then fills both one-event timelines, so both events reach the sections until
/// the reload. The original configuration is restored before the outcome is
/// judged, and both users leave again.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn forbidden_server_leaves_the_section(
	services: &Services,
	alice: &Client<'_>,
	bob: &Client<'_>,
	room: &RoomId,
) -> Result {
	let remote = UserId::parse(format!("@nina:{REMOTE}"))?;
	let forbidden = UserId::parse(format!("@mallory:{FORBIDDEN}"))?;
	let kept = remote_sticky(services, &remote, room, "remote-server").await?;
	let refused = remote_sticky(services, &forbidden, room, "forbidden-server").await?;

	alice
		.send_text(room, "after-remote", "after")
		.await?;

	for events in sections(bob, room).await? {
		let served = [&kept, &refused]
			.into_iter()
			.all(|event_id| find(&events, event_id).is_some_and(fresh_copy));

		require(served, "a section lost a remote sender's sticky event before the reload")?;
	}

	let deny_list = services
		.server
		.config
		.database_path
		.join("forbidden-server.toml");

	write(&deny_list, DENY_LIST)?;
	services
		.config
		.reload(once(deny_list.as_path()))?;

	let served = sections(bob, room).await;

	services.config.reload(empty())?;

	for events in served? {
		require(
			find(&events, &kept).is_some_and(fresh_copy),
			"a section lost a remote sender's sticky event",
		)?;

		require(
			find(&events, &refused).is_none(),
			"a section served a forbidden server's sticky event",
		)?;
	}

	let leave = RoomMemberEventContent::new(MembershipState::Leave);

	for user_id in [&remote, &forbidden] {
		append_as(services, user_id, room, PduBuilder::state(user_id.as_str(), &leave)).await?;
	}

	Ok(())
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

/// Joins a remote user to the room, then sends a sticky message under `key` as
/// that user.
///
/// Both events take the local build, so the remote server need not exist.
#[tracing::instrument(level = "debug", skip_all)]
async fn remote_sticky(
	services: &Services,
	user_id: &UserId,
	room: &RoomId,
	key: &str,
) -> Result<OwnedEventId> {
	let join = RoomMemberEventContent::new(MembershipState::Join);

	append_as(services, user_id, room, PduBuilder::state(user_id.as_str(), &join)).await?;
	append_as(services, user_id, room, sticky_text(key)?).await
}

/// Builds and appends an event as `user_id` under the room's state lock.
///
/// The local build signs it with this server's key whatever the sender's
/// server, standing in for an event that arrived over federation.
#[tracing::instrument(level = "debug", skip_all)]
async fn append_as(
	services: &Services,
	user_id: &UserId,
	room: &RoomId,
	builder: PduBuilder,
) -> Result<OwnedEventId> {
	let state_lock = services.state.mutex.lock(room).await;

	services
		.timeline
		.build_and_append_pdu(builder, user_id, room, &state_lock)
		.await
}
