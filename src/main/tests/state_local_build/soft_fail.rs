use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel_core::{
	Err, Result, async_noinline, err,
	ruma::{
		CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, UInt,
		UserId,
		events::{
			StateEventType,
			room::member::{MembershipState, RoomMemberEventContent},
		},
	},
	utils::result::NotFound,
};
use tuwunel_matrix::{PduEvent, pdu::PduBuilder};
use tuwunel_service::Services;

use super::{
	client::{Client, accepted, field, register},
	helpers::{
		SignedPdu, append_membership, append_message, create_room, redeliver,
		set_forward_extremities, set_forward_extremity, sign_message, sign_outlier_message,
		sign_state,
	},
	required_state::expect_state,
	sync::joined_room,
};

const STATE_LEAVER_TOKEN: &str = "state-local-build-soft-fail-state-access-token";
const MESSAGE_LEAVER_TOKEN: &str = "state-local-build-soft-fail-message-access-token";
const ADOPTER: &str = "adopts both leaves";
const SHARED: &str = "sent while the leaver is joined";
const MISSED: &str = "sent after the first leave";
const HIERARCHY_CACHE: &str = "roomid_spacehierarchy";

/// Which kind of event adopts both rival leaves.
///
/// A state event resolves the room's current state on its own; a message does
/// so only through the soft-failed prev it names.
#[derive(Clone, Copy)]
enum Descendant {
	State,
	Message,
}

// size firewall
#[async_noinline]
pub(super) async fn soft_failed_event_keeps_state_row<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let (first, first_json) = sign_leave(services, user_id, room_id, "first leave", None).await?;
	let (delayed, delayed_json) =
		sign_leave(services, user_id, room_id, "delayed leave", None).await?;

	let original_prev = match first.prev_events.as_slice() {
		| [prev] => prev,
		| [] => return Err!("first leave has no predecessor"),
		| _ => return Err!("first leave has multiple predecessors"),
	};

	let top_event_id = prepare_soft_fail_descendant(
		services,
		user_id,
		room_id,
		&delayed,
		&delayed_json,
		original_prev,
	)
	.await?;

	let accepted = redeliver(services, room_id, &first, first_json, "first leave").await?;

	assert!(accepted, "first leave was not accepted");

	let handled = redeliver(services, room_id, &delayed, delayed_json, "delayed leave").await?;

	assert!(!handled, "delayed leave was not soft failed");

	let delayed_id: &EventId = delayed.event_id.as_ref();
	let soft_failed = services
		.pdu_metadata
		.is_event_soft_failed(delayed_id)
		.await;

	assert!(soft_failed, "delayed leave lacks its soft-fail marker");

	let absent = services
		.timeline
		.non_outlier_pdu_exists(delayed_id)
		.await
		.is_not_found();

	assert!(absent, "soft-failed event reached the timeline");

	let retained = services.timeline.pdu_exists(delayed_id).await;

	assert!(retained, "soft-failed event disappeared from the outlier store");

	let shortstatehash = services
		.state
		.pdu_shortstatehash(delayed_id)
		.await?;

	let has_create = services
		.state_accessor
		.state_get_id(shortstatehash, &StateEventType::RoomCreate, "")
		.await
		.is_ok();

	assert!(has_create, "soft-fail state row has no create event");

	let has_membership = services
		.state_accessor
		.state_get_id(shortstatehash, &StateEventType::RoomMember, user_id.as_str())
		.await
		.is_ok();

	assert!(has_membership, "soft-fail state row has no positional membership");

	let report = services
		.event_handler
		.local_state_report(&top_event_id)
		.await?;

	assert_eq!(report.visited, 1, "descendant walk missed its held predecessor");
	assert_eq!(report.gate_drops, 1, "descendant walk did not fold the soft-failed predecessor");

	assert_eq!(report.fallback, None, "descendant walk fell back");
	assert!(report.state_len.is_some(), "descendant walk produced no state");

	Ok(())
}

/// A soft-failed state event that wins the state resolution of a later event
/// reaches the client in both sync protocols.
///
/// Each kind of event adopting the rival leaves runs in a room of its own, and
/// either kind moves the room's current state to the soft-failed leave. The
/// departed member still cannot read a message sent after their first leave.
// size firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn soft_failed_state_reaches_sync<'a>(
	services: &'a Services,
	base: &'a str,
	token: &'a str,
	user_id: &'a UserId,
) -> Result {
	let client = Client { services, base, token };

	adopted_by(&client, user_id, Descendant::State).await?;
	adopted_by(&client, user_id, Descendant::Message).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn adopted_by(client: &Client<'_>, user_id: &UserId, descendant: Descendant) -> Result {
	let &Client { services, base, token } = client;
	let (localpart, leaver_token) = match descendant {
		| Descendant::State => ("softfailstate", STATE_LEAVER_TOKEN),
		| Descendant::Message => ("softfailmessage", MESSAGE_LEAVER_TOKEN),
	};

	let room_id: &RoomId = &create_room(services, base, token).await?;
	let leaver = register(services, localpart, leaver_token).await?;

	append_membership(services, user_id, room_id, &leaver, MembershipState::Invite).await?;
	append_membership(services, &leaver, room_id, &leaver, MembershipState::Join).await?;

	let shared = append_message(services, user_id, room_id, SHARED).await?;
	let delayed = rival_leaves(services, &leaver, room_id).await?;
	let delayed_id: &EventId = delayed.event_id.as_ref();

	warm_hierarchy(client, room_id).await?;

	let missed = append_message(services, user_id, room_id, MISSED).await?;
	let kept = hierarchy_row(services, room_id).await.is_ok();

	assert!(kept, "a local message evicted the hierarchy summary");

	let baseline = client.get("sync?timeout=0").await?;
	let since = field(&baseline, "next_batch")?;
	let subscription = json!({
		"conn_id": localpart,
		"room_subscriptions": { room_id.as_str(): {
			"required_state": [["m.room.member", leaver.as_str()]],
			"timeline_limit": 1,
		} },
	});

	let opened = client.sync(None, &subscription).await?;
	let adopter =
		adopt_both(services, user_id, room_id, [&missed, delayed_id], descendant).await?;

	assert_member(services, room_id, &leaver, &adopter, delayed_id).await?;

	let evicted = hierarchy_row(services, room_id)
		.await
		.is_not_found();

	assert!(evicted, "adopting the soft-failed leave left the hierarchy summary cached");

	let departed = Client { services, base, token: leaver_token };

	assert_departed_view(&departed, room_id, &shared, &missed).await?;

	let incremental = client
		.get(&format!("sync?timeout=0&since={since}"))
		.await?;

	let room = joined_room(&incremental, room_id)?;

	assert!(
		event_ids(&room["timeline"]["events"]).any(|id| id == adopter.as_str()),
		"incremental timeline omitted the event adopting both leaves: {room}"
	);

	assert!(
		event_ids(&room["timeline"]["events"]).all(|id| id != delayed_id.as_str()),
		"incremental timeline relayed the soft-failed leave: {room}"
	);

	assert!(
		event_ids(&room["state"]["events"]).any(|id| id == delayed_id.as_str()),
		"incremental state omitted the soft-failed leave that won resolution: {room}"
	);

	let resumed = client.sync(Some(&opened), &subscription).await?;

	expect_state(
		&resumed,
		room_id,
		&[delayed_id.as_str()],
		"soft-failed leave that won resolution",
	)
}

async fn sign_leave(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	reason: &str,
	timestamp: Option<MilliSecondsSinceUnixEpoch>,
) -> Result<SignedPdu> {
	let content = RoomMemberEventContent {
		reason: Some(reason.to_owned()),
		..RoomMemberEventContent::new(MembershipState::Leave)
	};

	let builder = PduBuilder {
		timestamp,
		..PduBuilder::state(user_id.as_str(), &content)
	};

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}

// size firewall
#[async_noinline]
async fn prepare_soft_fail_descendant<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
	delayed: &'a PduEvent,
	delayed_json: &'a CanonicalJsonObject,
	original_prev: &'a EventId,
) -> Result<OwnedEventId> {
	services
		.timeline
		.add_pdu_outlier(&delayed.event_id, delayed_json);

	set_forward_extremity(services, room_id, delayed.event_id.as_ref()).await;

	// size firewall
	let (held, held_json) =
		Box::pin(sign_state(services, user_id, room_id, "held after leave")).await?;

	services
		.timeline
		.add_pdu_outlier(&held.event_id, &held_json);

	set_forward_extremity(services, room_id, held.event_id.as_ref()).await;

	// size firewall
	let (top, _) =
		Box::pin(sign_outlier_message(services, user_id, room_id, "top after leave")).await?;

	set_forward_extremity(services, room_id, original_prev).await;

	Ok(top.event_id)
}

/// Signs two leaves of one member on the same predecessor and returns the one
/// that soft-fails.
///
/// The delayed leave is sent first but delivered second, so it soft-fails, and
/// state resolution, ordering such rivals by timestamp, later keeps it.
async fn rival_leaves(
	services: &Services,
	leaver: &UserId,
	room_id: &RoomId,
) -> Result<PduEvent> {
	let (first, first_json) = sign_leave(services, leaver, room_id, "first leave", None).await?;
	let sent_earlier = first
		.origin_server_ts
		.checked_sub(UInt::from(1_u32))
		.map(MilliSecondsSinceUnixEpoch);

	let (delayed, delayed_json) =
		sign_leave(services, leaver, room_id, "delayed leave", sent_earlier).await?;

	let accepted = redeliver(services, room_id, &first, first_json, "first leave").await?;

	assert!(accepted, "first leave was not accepted");

	let handled = redeliver(services, room_id, &delayed, delayed_json, "delayed leave").await?;

	assert!(!handled, "delayed leave was not soft failed");

	Ok(delayed)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn warm_hierarchy(client: &Client<'_>, room_id: &RoomId) -> Result {
	let url = client.client_url("v1/", &format!("rooms/{room_id}/hierarchy"));

	accepted(client.get_url(&url).await?).await?;

	hierarchy_row(client.services, room_id)
		.await
		.map_err(|error| err!("hierarchy request cached no room summary: {error}"))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn hierarchy_row(services: &Services, room_id: &RoomId) -> Result {
	services.db[HIERARCHY_CACHE].exists(room_id).await
}

async fn adopt_both(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	prevs: [&EventId; 2],
	descendant: Descendant,
) -> Result<OwnedEventId> {
	set_forward_extremities(services, room_id, prevs).await;

	let (adopter, adopter_json) = match descendant {
		| Descendant::State => sign_state(services, user_id, room_id, ADOPTER).await?,
		| Descendant::Message => sign_message(services, user_id, room_id, ADOPTER).await?,
	};

	let accepted = redeliver(services, room_id, &adopter, adopter_json, "adopting event").await?;

	assert!(accepted, "event adopting both leaves was not accepted");

	Ok(adopter.event_id)
}

async fn assert_member(
	services: &Services,
	room_id: &RoomId,
	leaver: &UserId,
	adopter: &EventId,
	expected: &EventId,
) -> Result {
	let adopted = services.state.pdu_shortstatehash(adopter).await?;
	let resolved = services
		.state_accessor
		.state_get_id(adopted, &StateEventType::RoomMember, leaver.as_str())
		.await?;

	assert_eq!(resolved, expected, "state resolution did not keep the soft-failed leave");

	let current = services
		.state_accessor
		.room_state_get_id(room_id, &StateEventType::RoomMember, leaver.as_str())
		.await?;

	assert_eq!(current, expected, "room current state did not adopt the soft-failed leave");

	Ok(())
}

/// Asserts a departed member reads an event from their membership but not one
/// sent after they left.
///
/// The readable event rules out a refusal for any reason other than the
/// departure position.
#[tracing::instrument(level = "debug", skip_all)]
async fn assert_departed_view(
	departed: &Client<'_>,
	room_id: &RoomId,
	during: &EventId,
	after: &EventId,
) -> Result {
	let status = async |event_id: &EventId| {
		departed
			.get_url(&departed.url(&format!("rooms/{room_id}/event/{event_id}")))
			.await
			.map(|response| response.status())
	};

	let seen = status(during).await?;

	assert_eq!(seen, StatusCode::OK, "departed member lost a message from their membership");

	let hidden = status(after).await?;

	assert_eq!(
		hidden,
		StatusCode::NOT_FOUND,
		"departed member reads an event sent after leaving"
	);

	Ok(())
}

fn event_ids(events: &Value) -> impl Iterator<Item = &str> {
	events
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(|event| event["event_id"].as_str())
}
