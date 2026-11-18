#![cfg(test)]

//! Sticky events (MSC4354) and their sliding sync extension (MSC4480), on a
//! booted server.
//!
//! The cases cover discovery, the refused durations, the key and remaining
//! time on a timeline copy, the legacy sync section on initial and incremental
//! syncs with its deduplication against the timeline, the sliding sync
//! extension, a later joiner fetching a sticky event the room's history
//! visibility hides, transaction replay, both membership types (the legacy call
//! member and MSC4143's `rtc.member`), delayed sends fired by action and by
//! timer, ignored senders, lazy members, expiry and redaction, an erased
//! sender, a sticky thread root's bundle, a forbidden server's sender, and a
//! withheld event weighed again.

use std::time::Duration;

use reqwest::{Method, Response, StatusCode};
use serde_json::{
	Value, json,
	value::{RawValue as RawJsonValue, to_raw_value},
};
use tokio::time::sleep;
use tuwunel_core::{
	Result, err, implement,
	ruma::{
		EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, TransactionId, UserId,
		api::federation::transactions::send_transaction_message::v1::Request as TransactionRequest,
	},
};
use tuwunel_matrix::{event::Sticky, pdu::PduBuilder};
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, poll_until, register},
	federation::send,
	fixture::boot,
	require::require,
	served::{
		erased_sender_leaves_the_section, forbidden_server_leaves_the_section,
		sticky_root_bundles_only_seen_replies,
	},
	sign::sign,
	sync::joined_room,
};

mod client;
mod fixture;
mod sliding_sync;

#[expect(
	dead_code,
	reason = "Only the loopback transaction is sent from here."
)]
mod federation;

#[path = "fixture/sign.rs"]
mod sign;

#[path = "client/get.rs"]
mod get;

#[expect(
	dead_code,
	reason = "Only the direct actions are used from the membership helpers."
)]
#[path = "client/membership.rs"]
mod membership;

#[path = "client/message.rs"]
mod message;

#[path = "client/put.rs"]
mod put;

#[path = "client/require.rs"]
mod require;

#[path = "client/sync.rs"]
mod sync;

#[path = "sticky_events/served.rs"]
mod served;

const ALICE: &str = "sticky-events-alice-token-00000000";
const BOB: &str = "sticky-events-bob-token-0000000000";
const CAROL: &str = "sticky-events-carol-token-00000000";
const DAVE: &str = "sticky-events-dave-token-000000000";
const QUIET: [&str; 2] = ["logging.enable=false", "database.pool.max_workers=64"];
const CALL_MEMBER: &str = "org.matrix.msc3401.call.member";
const RTC_MEMBER: &str = "org.matrix.msc4143.rtc.member";
const DURATION: &str = "org.matrix.msc4354.sticky_duration_ms";
const STICKY_KEY: &str = "msc4354_sticky";
const TTL: &str = "msc4354_sticky_duration_ttl_ms";
const EXTENSION: &str = "org.matrix.msc4354.sticky_events";
const DURATION_MS: u64 = 600_000;
const UNSTABLE: &str = "unstable/org.matrix.msc4140/";

// Long enough to lapse well before the joiner looks, short enough to wait out.
const BRIEF_MS: u64 = 1_000;

// How long the worker may take to weigh a withheld event once its room changes.
const PROMOTION_DEADLINE: Duration = Duration::from_secs(10);

// Long enough for the worker to weigh a queued room's withheld events, had a
// message queued it.
const SETTLE: Duration = Duration::from_secs(2);

// The legacy and the MSC4143 membership types, each with a label for its transaction ids.
const MEMBER_TYPES: [(&str, &str); 2] = [(CALL_MEMBER, "call"), (RTC_MEMBER, "rtc")];

// The application every `rtc.member` join runs, and the slot each event names:
// an `m.rtc.slot` state key, the application type then `#` and the slot's id.
const APPLICATION: &str = "m.call";
const SLOT_ID: &str = "m.call#room";

// The sticky key and transaction id of the leave only its timer sends.
const TIMED_LEAVE: &str = "timed-leave";

// Short enough to wait out, so the delayed event fires by its timer alone.
const TIMER_DELAY_MS: u64 = 300;

// Far past the timer's delay, so a slow host is not mistaken for a timer that
// never fired.
const FIRE_DEADLINE: Duration = Duration::from_secs(10);

// `{"room":{"timeline":{"limit":1}}}`, encoded for a query string.
const ONE_EVENT: &str = "%7B%22room%22%3A%7B%22timeline%22%3A%7B%22limit%22%3A1%7D%7D%7D";

// `{"room":{"timeline":{"types":["m.room.message"]}}}`, encoded for a query string.
const MESSAGES_ONLY: &str =
	"%7B%22room%22%3A%7B%22timeline%22%3A%7B%22types%22%3A%5B%22m.room.message%22%5D%7D%7D%7D";

/// The parties of the withheld-event case.
///
/// Alice moderates the room, and the server delivers Carol's late events to
/// itself over the federation ingest, as a peer would.
struct Withheld<'a> {
	services: &'a Services,
	base: &'a str,
	alice: &'a Client<'a>,
	carol: &'a Client<'a>,
	carol_id: &'a UserId,
}

/// Runs every case on one server.
///
/// The cases share one room and build on the events the earlier ones send.
#[test]
fn sticky_events() -> Result { boot("sticky-events", QUIET, exercise) }

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	let alice_id = register(services, "alice", ALICE).await?;
	let bob_id = register(services, "bob", BOB).await?;
	let carol_id = register(services, "carol", CAROL).await?;

	register(services, "dave", DAVE).await?;

	let alice = Client { services, base, token: ALICE };
	let bob = Client { services, base, token: BOB };
	let carol = Client { services, base, token: CAROL };
	let dave = Client { services, base, token: DAVE };

	advertised(&alice).await?;

	// Joined history visibility, so only stickiness shows a later joiner an event.
	let room = alice
		.create_room(&json!({
			"preset": "public_chat",
			"initial_state": [{
				"type": "m.room.history_visibility",
				"state_key": "",
				"content": { "history_visibility": "joined" },
			}],
		}))
		.await?;

	refused_durations(&alice, &room).await?;

	let hidden = alice.send_text(&room, "hidden", "before").await?;
	let early = alice.send_sticky(&room, "early").await?;

	bob.act(&room, "join").await?;

	let last = alice.send_text(&room, "last", "after").await?;

	timeline_copy_carries_the_key(&alice, &room, &early).await?;

	let since = initial_sync_offers_the_section(&bob, &room, &early, &last).await?;
	let later = incremental_sync_offers_new_events(&alice, &bob, &room, &since, &early).await?;

	extension_offers_events_outside_the_timeline(&bob, &room, [&early, &later]).await?;
	later_joiner_fetches_only_the_sticky_event(&bob, &room, &early, &hidden).await?;
	retried_transaction_keeps_its_answer(&alice, &room).await?;
	member_types_reach_the_section(&alice, &bob, &room).await?;
	delayed_sticky_events_fire(&alice, &room).await?;
	delayed_sticky_leave_fires_by_timer(&alice, &room).await?;
	filtered_timeline_leaves_the_event_to_the_section(&alice, &bob, &room).await?;
	sliding_increment_offers_only_new_events(&alice, &bob, &room, &early).await?;
	lazy_members_include_sticky_senders(&bob, &alice_id, &room).await?;
	ignored_sender_is_left_out(&alice, &bob, &bob_id, &alice_id, &room, &early).await?;
	departed_member_loses_sight(&carol, &room, &early).await?;
	lapsed_and_redacted_events_lose_stickiness(&alice, &dave, &room, &early).await?;
	erased_sender_leaves_the_section(services, base, &alice, &room).await?;
	sticky_root_bundles_only_seen_replies(services, base, &alice, &bob, &bob_id, &room).await?;
	forbidden_server_leaves_the_section(services, &alice, &bob, &room).await?;

	let withheld = Withheld {
		services,
		base,
		alice: &alice,
		carol: &carol,
		carol_id: &carol_id,
	};

	withheld.waits_for_its_sender(&room).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn advertised(alice: &Client<'_>) -> Result {
	let versions = alice
		.get_url(&alice.client_url("", "versions"))
		.await?;

	let versions = accepted(versions).await?;

	require(
		versions["unstable_features"]["org.matrix.msc4354"] == true,
		"/versions does not advertise org.matrix.msc4354",
	)
}

/// A zero duration, and a duration on a delayed state event, are refused.
///
/// Both are refused with M_INVALID_PARAM before any event is built.
#[tracing::instrument(level = "debug", skip_all)]
async fn refused_durations(alice: &Client<'_>, room: &RoomId) -> Result {
	let zero = alice.url(&format!("rooms/{room}/send/{CALL_MEMBER}/zero?{DURATION}=0"));
	let zero = alice
		.request(Method::PUT, &zero, &json!({}))
		.await?;

	require(invalid_param(zero).await?, "a zero sticky duration was accepted")?;

	let delayed = alice.client_url(
		"unstable/org.matrix.msc4140/",
		&format!("rooms/{room}/delayed_event/m.room.topic/sticky-state"),
	);

	let body = json!({
		"delay_ms": 60_000,
		"state_key": "",
		"content": { "topic": "sticky" },
		"org.matrix.msc4354.sticky_duration_ms": DURATION_MS,
	});

	let delayed = alice
		.request(Method::PUT, &delayed, &body)
		.await?;

	require(invalid_param(delayed).await?, "a sticky delayed state event was accepted")
}

#[tracing::instrument(level = "debug", skip_all)]
async fn timeline_copy_carries_the_key(
	alice: &Client<'_>,
	room: &RoomId,
	sticky: &EventId,
) -> Result {
	let sync = alice.get("sync?timeout=0").await?;
	let timeline = &joined_room(&sync, room)?["timeline"]["events"];
	let copy = find(timeline, sticky)
		.ok_or_else(|| err!("timeline omitted the sticky event {sticky}"))?;

	require(fresh_copy(copy), "the timeline copy lacks the sticky key or its remaining time")
}

/// On an initial sync, a sticky event sent before the join reaches the section
/// when the timeline does not carry it.
///
/// The filter limits the timeline to its last event, so the earlier sticky
/// event can only arrive in the section.
#[tracing::instrument(level = "debug", skip_all)]
async fn initial_sync_offers_the_section(
	bob: &Client<'_>,
	room: &RoomId,
	sticky: &EventId,
	last: &EventId,
) -> Result<String> {
	let sync = bob
		.get(&format!("sync?timeout=0&filter={ONE_EVENT}"))
		.await?;

	let joined = joined_room(&sync, room)?;
	let section = &joined[STICKY_KEY]["events"];

	require(
		find(&joined["timeline"]["events"], last).is_some(),
		"the timeline lost its last event",
	)?;

	require(
		find(section, sticky).is_some_and(fresh_copy),
		"the section omitted the sticky event",
	)?;

	require(find(section, last).is_none(), "the section carried a plain event")?;

	let next_batch = field(&sync, "next_batch")?.to_owned();

	Ok(next_batch)
}

/// An incremental sync offers only sticky events sent since, and none the
/// timeline already carries.
///
/// The first sync limits the timeline to one event, so the new sticky event
/// arrives in the section; the second leaves the timeline whole, so the
/// section must not repeat what it carries.
#[tracing::instrument(level = "debug", skip_all)]
async fn incremental_sync_offers_new_events(
	alice: &Client<'_>,
	bob: &Client<'_>,
	room: &RoomId,
	since: &str,
	early: &EventId,
) -> Result<OwnedEventId> {
	let later = alice.send_sticky(room, "later").await?;
	let last = alice
		.send_text(room, "last-again", "again")
		.await?;

	let sync = bob
		.get(&format!("sync?timeout=0&since={since}&filter={ONE_EVENT}"))
		.await?;

	let joined = joined_room(&sync, room)?;
	let section = &joined[STICKY_KEY]["events"];

	require(
		find(&joined["timeline"]["events"], &last).is_some(),
		"the timeline lost its event",
	)?;

	require(
		find(section, &later).is_some_and(fresh_copy),
		"the section omitted the new event",
	)?;

	require(find(section, early).is_none(), "the section repeated an event already offered")?;

	let since = field(&sync, "next_batch")?;
	let shown = alice.send_sticky(room, "shown").await?;
	let sync = bob
		.get(&format!("sync?timeout=0&since={since}"))
		.await?;

	let joined = joined_room(&sync, room)?;

	require(
		find(&joined["timeline"]["events"], &shown).is_some(),
		"the timeline lost its event",
	)?;

	require(
		find(&joined[STICKY_KEY]["events"], &shown).is_none(),
		"the section repeated an event the timeline carries",
	)?;

	Ok(later)
}

/// A new sliding sync connection is offered every unexpired sticky event its
/// timeline does not carry.
///
/// The connection is new, so the extension owes each one whenever it was sent.
#[tracing::instrument(level = "debug", skip_all)]
async fn extension_offers_events_outside_the_timeline<'a, Expected>(
	bob: &Client<'_>,
	room: &RoomId,
	expected: Expected,
) -> Result
where
	Expected: IntoIterator<Item = &'a OwnedEventId>,
{
	let body = json!({
		"lists": { "all": { "ranges": [[0, 9]], "timeline_limit": 1 } },
		"extensions": { "org.matrix.msc4354.sticky_events": { "enabled": true } },
	});

	let sync = bob.sliding_sync(None, 0, &body).await?;
	let events = &sync["extensions"][EXTENSION]["rooms"][room.as_str()]["events"];

	for event_id in expected {
		require(
			find(events, event_id).is_some_and(fresh_copy),
			"the extension omitted a sticky event",
		)?;
	}

	Ok(())
}

/// A joined user fetches a sticky event sent before the join, while the
/// room's history visibility still hides a plain one.
///
/// The room shows a member only events from after the join, so stickiness
/// alone makes the earlier one visible.
#[tracing::instrument(level = "debug", skip_all)]
async fn later_joiner_fetches_only_the_sticky_event(
	bob: &Client<'_>,
	room: &RoomId,
	sticky: &EventId,
	hidden: &EventId,
) -> Result {
	let sticky = bob
		.get_url(&bob.url(&format!("rooms/{room}/event/{sticky}")))
		.await?;

	let hidden = bob
		.get_url(&bob.url(&format!("rooms/{room}/event/{hidden}")))
		.await?;

	require(sticky.status().is_success(), "a joined user could not fetch a sticky event")?;
	require(
		!hidden.status().is_success(),
		"history visibility no longer hides a plain event",
	)
}

/// A retried transaction is answered with its first event, whatever duration
/// the retry carries.
///
/// The duration is checked only once no earlier send answers the transaction,
/// so a zero on the retry is not refused.
#[tracing::instrument(level = "debug", skip_all)]
async fn retried_transaction_keeps_its_answer(alice: &Client<'_>, room: &RoomId) -> Result {
	let first = alice.send_sticky(room, "retried").await?;
	let retry = alice
		.put(
			&format!("rooms/{room}/send/{CALL_MEMBER}/retried?{DURATION}=0"),
			&json!({ "msc4354_sticky_key": "retried" }),
		)
		.await?;

	require(
		field(&retry, "event_id")? == first.as_str(),
		"a retried transaction lost its answer",
	)
}

/// A sticky membership of either type reaches the section of an incremental
/// sync whose timeline it falls outside.
///
/// The legacy call member and MSC4143's `rtc.member` each send an event and
/// then a message, so a one-event timeline leaves the membership to the
/// section.
#[tracing::instrument(level = "debug", skip_all)]
async fn member_types_reach_the_section(
	alice: &Client<'_>,
	bob: &Client<'_>,
	room: &RoomId,
) -> Result {
	for (event_type, label) in MEMBER_TYPES {
		let since = bob.get("sync?timeout=0").await?;
		let since = field(&since, "next_batch")?;
		let key = format!("{label}-member");
		let member = alice
			.send_sticky_for(room, event_type, &key, DURATION_MS)
			.await?;

		let last = alice
			.send_text(room, &format!("{label}-text"), "after")
			.await?;

		let sync = bob
			.get(&format!("sync?timeout=0&since={since}&filter={ONE_EVENT}"))
			.await?;

		let joined = joined_room(&sync, room)?;
		let expected = member_content(event_type, &key, "join");

		require(
			find(&joined["timeline"]["events"], &last).is_some(),
			"the timeline lost its event",
		)?;

		require(
			find(&joined[STICKY_KEY]["events"], &member)
				.is_some_and(|event| fresh_copy_of(event, &expected)),
			"the section omitted a sticky membership or changed its content",
		)?;
	}

	Ok(())
}

/// Content for a sticky membership of either type, keyed by `key`.
///
/// An `rtc.member` event follows MSC4143's schema: its slot, a member whose id
/// is the sticky key, and the application on a join or the reason on a leave.
/// A legacy call member carries the sticky key alone, as elsewhere in the suite.
fn member_content(event_type: &str, key: &str, membership: &str) -> Value {
	let member = json!({ "id": key, "membership": membership });

	match (event_type, membership) {
		| (RTC_MEMBER, "join") => json!({
			"slot_id": SLOT_ID,
			"member": member,
			"application": { "type": APPLICATION },
			"msc4354_sticky_key": key,
		}),
		| (RTC_MEMBER, _) => json!({
			"slot_id": SLOT_ID,
			"member": member,
			"leave_reason": { "code": "delayed_leave" },
			"msc4354_sticky_key": key,
		}),
		| _ => json!({ "msc4354_sticky_key": key }),
	}
}

/// Whether a copy is fresh and carries the content it was sent with.
///
/// The server keeps a sticky event's content as sent, so any difference is a
/// defect in delivery rather than a client's edit.
fn fresh_copy_of(event: &Value, content: &Value) -> bool {
	fresh_copy(event) && event["content"] == *content
}

/// A delayed sticky event of either membership type, from either route, is
/// sent with its sticky object and a remaining time counted from when it fires.
///
/// Each is scheduled well in the future and fired at once through its send
/// action, then read back from the timeline.
#[tracing::instrument(level = "debug", skip_all)]
async fn delayed_sticky_events_fire(alice: &Client<'_>, room: &RoomId) -> Result {
	for (event_type, label) in MEMBER_TYPES {
		fire_delayed_sticky_events(alice, room, event_type, label).await?;
	}

	let sync = alice.get("sync?timeout=0").await?;
	let timeline = &joined_room(&sync, room)?["timeline"]["events"];

	for (event_type, label) in MEMBER_TYPES {
		for route in ["delayed-query", "delayed-body"] {
			let key = format!("{label}-{route}");
			let expected = member_content(event_type, &key, "leave");

			require(
				find_keyed(timeline, &key).is_some_and(|event| fresh_copy_of(event, &expected)),
				"a fired delayed event lacks the sticky key, its remaining time or its content",
			)?;
		}
	}

	Ok(())
}

/// Schedule one delayed sticky event of a type through each route, then fire
/// both through their send actions.
///
/// The label keeps each type's transaction ids and sticky keys apart.
#[tracing::instrument(level = "debug", skip_all)]
async fn fire_delayed_sticky_events(
	alice: &Client<'_>,
	room: &RoomId,
	event_type: &str,
	label: &str,
) -> Result {
	let query_key = format!("{label}-delayed-query");
	let by_query = format!(
		"rooms/{room}/send/{event_type}/{query_key}?org.matrix.msc4140.delay=60000&\
		 {DURATION}={DURATION_MS}"
	);

	let by_query = alice
		.put(&by_query, &member_content(event_type, &query_key, "leave"))
		.await?;

	let body_key = format!("{label}-delayed-body");
	let by_body = alice
		.client_url(UNSTABLE, &format!("rooms/{room}/delayed_event/{event_type}/{body_key}"));

	let body = json!({
		"delay_ms": 60_000,
		"content": member_content(event_type, &body_key, "leave"),
		DURATION: DURATION_MS,
	});

	let by_body = alice
		.request(Method::PUT, &by_body, &body)
		.await?;

	let by_body = accepted(by_body).await?;

	for reply in [&by_query, &by_body] {
		let action = format!("delayed_events/{}/send", field(reply, "delay_id")?);

		alice
			.post_json(&alice.client_url(UNSTABLE, &action), &json!({}))
			.await?;
	}

	Ok(())
}

/// A delayed sticky leave fires by its timer, with its sticky object and the
/// content it was scheduled with.
///
/// The `rtc.member` leave names its slot and member as MSC4143 requires, and no
/// send action touches it, so only the timer can send it.
#[tracing::instrument(level = "debug", skip_all)]
async fn delayed_sticky_leave_fires_by_timer(alice: &Client<'_>, room: &RoomId) -> Result {
	let path = format!(
		"rooms/{room}/send/{RTC_MEMBER}/{TIMED_LEAVE}?org.matrix.msc4140.delay={TIMER_DELAY_MS}&\
		 {DURATION}={DURATION_MS}"
	);

	let reply = alice
		.put(&path, &member_content(RTC_MEMBER, TIMED_LEAVE, "leave"))
		.await?;

	field(&reply, "delay_id")?;

	let fired = poll_until(FIRE_DEADLINE, async || timed_leave_sent(alice, room).await).await;

	require(fired, "a delayed sticky leave never fired by its timer")
}

#[tracing::instrument(level = "debug", skip_all)]
async fn timed_leave_sent(alice: &Client<'_>, room: &RoomId) -> bool {
	let Ok(sync) = alice.get("sync?timeout=0").await else {
		return false;
	};

	let expected = member_content(RTC_MEMBER, TIMED_LEAVE, "leave");

	joined_room(&sync, room).is_ok_and(|joined| {
		find_keyed(&joined["timeline"]["events"], TIMED_LEAVE)
			.is_some_and(|event| fresh_copy_of(event, &expected))
	})
}

/// A sticky event the timeline filter leaves out reaches the section.
///
/// The filter admits messages alone, so the sticky event lies within the
/// timeline's range yet only the section can carry it.
#[tracing::instrument(level = "debug", skip_all)]
async fn filtered_timeline_leaves_the_event_to_the_section(
	alice: &Client<'_>,
	bob: &Client<'_>,
	room: &RoomId,
) -> Result {
	let since = bob.get("sync?timeout=0").await?;
	let since = field(&since, "next_batch")?;
	let filtered = alice.send_sticky(room, "filtered").await?;

	alice
		.send_text(room, "filtered-text", "text")
		.await?;

	let sync = bob
		.get(&format!("sync?timeout=0&since={since}&filter={MESSAGES_ONLY}"))
		.await?;

	let joined = joined_room(&sync, room)?;

	require(
		find(&joined["timeline"]["events"], &filtered).is_none(),
		"the timeline filter let the sticky event through",
	)?;

	require(
		find(&joined[STICKY_KEY]["events"], &filtered).is_some_and(fresh_copy),
		"the section omitted a sticky event the timeline filter left out",
	)
}

/// A sliding sync connection's next response offers only the sticky events
/// sent since, and none its timeline carries.
///
/// The second response follows a sticky event and a plain one that fills the
/// one-event timeline; the third follows a sticky event the timeline carries.
#[tracing::instrument(level = "debug", skip_all)]
async fn sliding_increment_offers_only_new_events(
	alice: &Client<'_>,
	bob: &Client<'_>,
	room: &RoomId,
	earlier: &EventId,
) -> Result {
	let body = extension_request(&json!([]));
	let start = bob.sliding_sync(None, 0, &body).await?;
	let fresh = alice.send_sticky(room, "sliding").await?;

	alice
		.send_text(room, "sliding-tip", "tip")
		.await?;

	let next = bob
		.sliding_sync(Some(field(&start, "pos")?), 0, &body)
		.await?;

	let events = extension_events(&next, room);

	require(
		find(events, &fresh).is_some_and(fresh_copy),
		"the increment omitted a new event",
	)?;

	require(find(events, earlier).is_none(), "the increment repeated an offered event")?;

	let shown = alice.send_sticky(room, "sliding-shown").await?;
	let last = bob
		.sliding_sync(Some(field(&next, "pos")?), 0, &body)
		.await?;

	require(
		find(&last["rooms"][room.as_str()]["timeline"], &shown).is_some(),
		"the sliding timeline lost its event",
	)?;

	require(
		find(extension_events(&last, room), &shown).is_none(),
		"the extension repeated an event the timeline carries",
	)
}

/// A sticky event's sender counts as a timeline sender for lazy-loaded
/// members.
///
/// Bob's own message fills the one-event timeline, so Alice's membership is
/// owed only for her sticky events.
#[tracing::instrument(level = "debug", skip_all)]
async fn lazy_members_include_sticky_senders(
	bob: &Client<'_>,
	alice_id: &UserId,
	room: &RoomId,
) -> Result {
	bob.send_text(room, "lazy-tip", "tip").await?;

	let body = extension_request(&json!([["m.room.member", "$LAZY"]]));
	let sync = bob.sliding_sync(None, 0, &body).await?;
	let lazy = sync["rooms"][room.as_str()]["required_state"]
		.as_array()
		.into_iter()
		.flatten()
		.any(|event| event["type"] == "m.room.member" && event["state_key"] == alice_id.as_str());

	require(lazy, "lazy members left out a sticky event's sender")
}

/// No timeline, section or extension delivers an ignored sender's sticky
/// events.
///
/// Bob ignores Alice for this case alone, so later cases see her events again.
/// Her newest sticky event fills both one-event timelines, so only the ignore
/// keeps it out of them.
#[tracing::instrument(level = "debug", skip_all)]
async fn ignored_sender_is_left_out(
	alice: &Client<'_>,
	bob: &Client<'_>,
	bob_id: &UserId,
	alice_id: &UserId,
	room: &RoomId,
	early: &EventId,
) -> Result {
	let ignored = format!("user/{bob_id}/account_data/m.ignored_user_list");

	bob.put(&ignored, &json!({ "ignored_users": { alice_id.as_str(): {} } }))
		.await?;

	let newest = alice.send_sticky(room, "while-ignored").await?;
	let sync = bob
		.get(&format!("sync?timeout=0&filter={ONE_EVENT}"))
		.await?;

	let joined = joined_room(&sync, room)?;
	let extension = bob
		.sliding_sync(None, 0, &extension_request(&json!([])))
		.await?;

	let timelines =
		[&joined["timeline"]["events"], &extension["rooms"][room.as_str()]["timeline"]];

	let sections = [&joined[STICKY_KEY]["events"], extension_events(&extension, room)];

	for events in timelines {
		require(
			find(events, &newest).is_none(),
			"a timeline delivered an ignored sender's event",
		)?;
	}

	for events in sections {
		require(
			find(events, early).is_none() && find(events, &newest).is_none(),
			"a sticky section delivered an ignored sender's event",
		)?;
	}

	bob.put(&ignored, &json!({ "ignored_users": {} }))
		.await
		.map(drop)
}

/// A member sees a sticky event from before the join while joined, through
/// `/event` and `/context`, and loses sight of it on leaving.
///
/// The extension of a departed member's new connection offers the room
/// nothing either.
#[tracing::instrument(level = "debug", skip_all)]
async fn departed_member_loses_sight(
	carol: &Client<'_>,
	room: &RoomId,
	early: &EventId,
) -> Result {
	let event = carol.url(&format!("rooms/{room}/event/{early}"));
	let context = carol.url(&format!("rooms/{room}/context/{early}?limit=0"));

	carol.act(room, "join").await?;

	let joined = carol.get_url(&event).await?;
	let around = carol.get_url(&context).await?;

	require(joined.status().is_success(), "a joined member could not fetch a sticky event")?;
	require(
		around.status().is_success(),
		"a joined member could not see a sticky event's context",
	)?;

	carol.act(room, "leave").await?;

	let departed = carol.get_url(&event).await?;
	let extension = carol
		.sliding_sync(None, 0, &extension_request(&json!([])))
		.await?;

	require(
		!departed.status().is_success(),
		"a departed member still fetched a sticky event",
	)?;

	require(
		find(extension_events(&extension, room), early).is_none(),
		"a departed member's extension offered a sticky event",
	)
}

/// An expired or redacted sticky event is no longer sticky: a later joiner can
/// neither fetch it nor find it in the section.
///
/// Both are sent before Dave joins, so the room's history visibility hides
/// them once stickiness no longer exempts them; an older event still sticky
/// stays in the section.
#[tracing::instrument(level = "debug", skip_all)]
async fn lapsed_and_redacted_events_lose_stickiness(
	alice: &Client<'_>,
	dave: &Client<'_>,
	room: &RoomId,
	early: &EventId,
) -> Result {
	let brief = alice
		.send_sticky_for(room, CALL_MEMBER, "brief", BRIEF_MS)
		.await?;

	let doomed = alice.send_sticky(room, "doomed").await?;

	alice
		.put(&format!("rooms/{room}/redact/{doomed}/redact-doomed"), &json!({}))
		.await?;

	sleep(Duration::from_millis(BRIEF_MS.saturating_mul(2))).await;
	dave.act(room, "join").await?;

	let sync = dave
		.get(&format!("sync?timeout=0&filter={ONE_EVENT}"))
		.await?;

	let section = &joined_room(&sync, room)?[STICKY_KEY]["events"];

	require(
		find(section, early).is_some_and(fresh_copy),
		"the section lost an unexpired event",
	)?;

	for lost in [&brief, &doomed] {
		let fetched = dave
			.get_url(&dave.url(&format!("rooms/{room}/event/{lost}")))
			.await?;

		require(!fetched.status().is_success(), "a joiner fetched an event no longer sticky")?;
		require(find(section, lost).is_none(), "the section offered an event no longer sticky")?;
	}

	Ok(())
}

/// A sticky event refused only by the room's current state is accepted once
/// that state lets its sender back in, unless it was redacted meanwhile.
///
/// Carol's events are built while she is joined and delivered after Alice bans
/// her; her return is the state change that weighs them again. An ordinary
/// message in between leaves the state as it was and weighs nothing.
#[implement(Withheld, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn waits_for_its_sender(&self, room: &RoomId) -> Result {
	let Self { services, alice, carol, carol_id, .. } = self;

	carol.act(room, "join").await?;

	let kept = self.signed_sticky(room, "kept").await?;
	let redacted = self.signed_sticky(room, "redacted").await?;

	alice.act_on(room, "ban", carol_id).await?;

	for (event_id, pdu) in [&kept, &redacted] {
		self.deliver(event_id, pdu.clone()).await?;

		let withheld = services
			.pdu_metadata
			.is_event_soft_failed(event_id)
			.await;

		require(
			withheld && !accepted_event(services, event_id).await,
			"a late event was not withheld",
		)?;
	}

	let fetched = alice
		.get(&format!("rooms/{room}/event/{}", kept.0))
		.await?;

	require(
		fetched.get(STICKY_KEY).is_none() && fetched["unsigned"].get(TTL).is_none(),
		"a withheld event was served as sticky",
	)?;

	let weighed = services
		.event_handler
		.backoff_metrics()
		.upgrade
		.lookups();

	alice.send_text(room, "unweighed", "text").await?;

	sleep(SETTLE).await;

	let unweighed = services
		.event_handler
		.backoff_metrics()
		.upgrade
		.lookups()
		.eq(&weighed);

	require(unweighed, "an unchanged room state weighed a withheld event again")?;

	alice
		.put(&format!("rooms/{room}/redact/{}/redact-withheld", redacted.0), &json!({}))
		.await?;

	alice.act_on(room, "unban", carol_id).await?;
	carol.act(room, "join").await?;

	let promoted =
		poll_until(PROMOTION_DEADLINE, async || accepted_event(services, &kept.0).await).await;

	require(promoted, "a withheld event was not accepted once its sender returned")?;

	let fetched = alice
		.get(&format!("rooms/{room}/event/{}", kept.0))
		.await?;

	require(fresh_copy(&fetched), "an accepted event was served without its sticky time")?;
	require(
		!accepted_event(services, &redacted.0).await,
		"a withheld event redacted meanwhile was accepted",
	)
}

/// Builds and signs one of Carol's sticky events against the room's current
/// state, without appending it.
///
/// Held back and delivered after a later state change, the event arrives as a
/// peer's would after crossing that change in flight.
#[implement(Withheld, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn signed_sticky(
	&self,
	room: &RoomId,
	key: &str,
) -> Result<(OwnedEventId, Box<RawJsonValue>)> {
	// Carol holds the default power level, below what call membership needs.
	let builder = sticky_text(key)?;
	let (event, json) = sign(self.services, self.carol_id, room, builder).await?;
	let pdu = self
		.services
		.federation
		.format_pdu_into(json, None)
		.await;

	Ok((event.event_id, pdu))
}

/// A sticky message under `key`, built for the local event path.
///
/// Any member may send one, unlike the call membership the other cases send.
fn sticky_text(key: &str) -> Result<PduBuilder> {
	let content = json!({ "msgtype": "m.text", "body": key, "msc4354_sticky_key": key });
	let builder = PduBuilder {
		event_type: "m.room.message".into(),
		content: to_raw_value(&content)?.into(),
		sticky: Some(Sticky { duration_ms: DURATION_MS }),
		..PduBuilder::default()
	};

	Ok(builder)
}

/// Delivers one event over the federation ingest, from this server to itself.
///
/// The transaction takes the path a peer's push takes, and must come back with
/// no error recorded for the event; a withheld event is not an error there.
#[implement(Withheld, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn deliver(&self, event_id: &EventId, pdu: Box<RawJsonValue>) -> Result {
	let origin = self.services.globals.server_name().to_owned();
	let now = MilliSecondsSinceUnixEpoch::now();
	let mut request = TransactionRequest::new(TransactionId::new(), origin, now); // non_exhaustive

	request.pdus = vec![pdu];

	let (status, body) = send(self.services, self.base, request).await?;
	let body: Value = serde_json::from_str(&body)?;

	require(status == 200, "the federation ingest refused the transaction")?;
	require(body["pdus"][event_id.as_str()] == json!({}), "the ingest reported an error")
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn send_sticky(&self, room: &RoomId, txn_id: &str) -> Result<OwnedEventId> {
	self.send_sticky_for(room, CALL_MEMBER, txn_id, DURATION_MS)
		.await
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn send_sticky_for(
	&self,
	room: &RoomId,
	event_type: &str,
	txn_id: &str,
	duration_ms: u64,
) -> Result<OwnedEventId> {
	let path = format!("rooms/{room}/send/{event_type}/{txn_id}?{DURATION}={duration_ms}");
	let response = self
		.put(&path, &member_content(event_type, txn_id, "join"))
		.await?;

	let event_id = field(&response, "event_id")?.try_into()?;

	Ok(event_id)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn accepted_event(services: &Services, event_id: &EventId) -> bool {
	services
		.timeline
		.non_outlier_pdu_exists(event_id)
		.await
		.is_ok()
}

/// A new connection's request: one list over every room, a one-event
/// timeline, the given required state, and the sticky events extension.
///
/// The one-event timeline leaves older sticky events to the extension, which
/// is where the cases look for them.
fn extension_request(required_state: &Value) -> Value {
	json!({
		"lists": {
			"all": {
				"ranges": [[0, 9]],
				"timeline_limit": 1,
				"required_state": required_state,
			},
		},
		"extensions": { EXTENSION: { "enabled": true } },
	})
}

fn extension_events<'a>(response: &'a Value, room: &RoomId) -> &'a Value {
	&response["extensions"][EXTENSION]["rooms"][room.as_str()]["events"]
}

#[tracing::instrument(level = "debug", skip_all)]
async fn invalid_param(response: Response) -> Result<bool> {
	let status = response.status();
	let body: Value = response.json().await?;

	Ok(status == StatusCode::BAD_REQUEST && body["errcode"] == "M_INVALID_PARAM")
}

fn find<'a>(events: &'a Value, event_id: &EventId) -> Option<&'a Value> {
	events
		.as_array()?
		.iter()
		.find(|event| event["event_id"] == event_id.as_str())
}

fn find_keyed<'a>(events: &'a Value, key: &str) -> Option<&'a Value> {
	events
		.as_array()?
		.iter()
		.find(|event| event["content"]["msc4354_sticky_key"] == key)
}

/// Whether a copy carries the sticky key with its duration and a remaining time
/// within it.
///
/// The test reads each copy moments after sending, so the remaining time is
/// positive and no longer than the duration.
fn fresh_copy(event: &Value) -> bool {
	let ttl = event["unsigned"][TTL].as_u64();

	event[STICKY_KEY]["duration_ms"] == DURATION_MS
		&& ttl.is_some_and(|ttl| ttl > 0 && ttl <= DURATION_MS)
}
