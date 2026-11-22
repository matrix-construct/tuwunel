use std::{fmt::Debug, iter::once, sync::Arc, time::Duration};

use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId,
	RoomVersionId, ServerName, api::error::ErrorKind, event_id, events::StateEventType, room_id,
	server_name, user_id,
};
use serde_json::{Value, from_value, json};
use tokio::time::timeout;
use tuwunel_core::{Result, config::Figment, err};
use tuwunel_matrix::{PduEvent, room_version::rules as room_version_rules};

use super::{
	super::{handle_incoming_pdu::Handled, handle_prev_pdu::PrevUpgrade},
	check_create_reference, check_local_join_sender,
};
use crate::{
	Services,
	rooms::state_compressor::CompressedStateEvent,
	test_utils::{
		StateEvent, creator_join, fixture, other_create, pdu_json, remote_key, remote_signed,
		room, save_room_state, signed_state, state_event, store_outlier, with_field,
		with_state_key,
	},
};

/// A create event must be the room's known create.
///
/// In room version 11 every other event must also name the known create among
/// its auth events.
#[test]
fn a_known_create_is_the_room_anchor() -> Result {
	let id = event_id!("$create");
	let room = room_id!("!room:example.com");
	let alice = user_id!("@alice:example.com");
	let rules = room_version_rules(&RoomVersionId::V11)?;
	let create: PduEvent =
		from_value(with_state_key(pdu_json(id, room, alice, "m.room.create", &json!({})), ""))?;

	check_create_reference(&create, &rules, id)?;
	check_create_reference(&create, &rules, event_id!("$different"))
		.expect_err("different create ID");

	let name = json!({ "name": "Example" });
	let name = pdu_json(event_id!("$name"), room, alice, "m.room.name", &name);
	let name: PduEvent =
		from_value(with_state_key(with_field(name, "auth_events", json!([id])), ""))?;

	check_create_reference(&name, &rules, id)?;
	check_create_reference(&name, &rules, event_id!("$different"))
		.expect_err("different auth anchor");

	Ok(())
}

/// In room version 12 the room ID names the create event, so no other event
/// needs to list it among its auth events.
///
/// The same event without the reference fails the room version 11 check.
#[test]
fn a_v12_event_names_its_create_through_the_room() -> Result {
	let create_id = event_id!("$create");
	let alice = user_id!("@alice:example.com");
	let room_id = room_id!("!room:example.com");
	let name = json!({ "name": "Example" });
	let name = pdu_json(event_id!("$name"), room_id, alice, "m.room.name", &name);
	let name: PduEvent = from_value(with_state_key(name, ""))?;

	check_create_reference(&name, &room_version_rules(&RoomVersionId::V12)?, create_id)?;
	check_create_reference(&name, &room_version_rules(&RoomVersionId::V11)?, create_id)
		.expect_err("room version 11 needs the reference");

	Ok(())
}

/// A local user's join must be sent by that user; other memberships, and joins
/// for another server's users, are left to the room's rules.
///
/// A membership that does not parse fails the check.
#[test]
fn a_local_join_is_self_sent() -> Result {
	let alice = user_id!("@alice:example.com");
	let local = server_name!("example.com");
	let member = |key: &str, membership: Value| -> Result<PduEvent> {
		let content = json!({ "membership": membership });
		let room_id = room_id!("!room:example.com");
		let event = pdu_json(event_id!("$member"), room_id, alice, "m.room.member", &content);

		Ok(from_value(with_state_key(event, key))?)
	};

	check_local_join_sender(&member(alice.as_str(), json!("join"))?, local)?;
	check_local_join_sender(&member("@bob:remote.example", json!("join"))?, local)?;

	for membership in ["invite", "leave", "ban"] {
		check_local_join_sender(&member("@bob:example.com", json!(membership))?, local)?;
	}

	check_local_join_sender(&member("@bob:example.com", json!("join"))?, local)
		.expect_err("different local user");

	check_local_join_sender(&member("@bob:example.com", json!(5))?, local)
		.expect_err("unparsable membership");

	Ok(())
}

/// The known create comes from the room's current state.
///
/// Before the room has state there is none. A state without a create event, an
/// unreadable reverse mapping of the create's short ID, or an unreadable state
/// pointer fails the read.
#[tokio::test]
async fn the_known_create_is_read_from_current_state() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let handler = &services.event_handler;
	let room_id = room_id!("!known:localhost");

	assert_eq!(handler.known_create_id(room_id).await?, None, "no state yet");

	let topic = json!({ "topic": "no create" });
	let (topic_id, topic) = signed_state(services, room_id, "m.room.topic", "", &topic)?;
	let topic = store_outlier(services, &StateEventType::RoomTopic, "", &topic_id, topic).await?;

	save_room_state(services, room_id, Arc::new([topic].into())).await?;

	handler
		.known_create_id(room_id)
		.await
		.expect_err("a state without a create");

	let [create_id, _] = existing_room(services, room_id).await?;
	let known = Some(create_id.clone());
	let reverse = services
		.short
		.get_shorteventid(&create_id)
		.await?
		.to_be_bytes();

	assert_eq!(handler.known_create_id(room_id).await?, known);

	services.db["shorteventid_eventid"].insert(&reverse, b"\xff");

	handler
		.known_create_id(room_id)
		.await
		.expect_err("an unreadable reverse mapping");

	services.db["shorteventid_eventid"].insert(&reverse, create_id.as_bytes());

	assert_eq!(handler.known_create_id(room_id).await?, known, "the mapping restored");

	services.db["roomid_shortstatehash"].insert(room_id.as_bytes(), b"bad");

	handler
		.known_create_id(room_id)
		.await
		.expect_err("an unreadable state pointer");

	Ok(())
}

/// An event admitted as an outlier to a room this server holds must name the
/// room's create event.
///
/// Another create event for the room is refused before it is stored. An event
/// whose auth events name a previously stored alternate create is refused too,
/// and so is an event naming no create.
#[tokio::test]
async fn an_outlier_must_name_the_known_create() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!anchored:localhost");
	let local = server_name!("localhost");
	let [create_id, join_id] = existing_room(services, room_id).await?;
	let (other_id, other) = other_create(services, room_id)?;
	let (named_id, named) = state_event(services, room_id, StateEvent {
		kind: "m.room.name",
		state_key: "",
		content: &json!({ "name": "anchored" }),
		auth_events: &[&create_id, &join_id],
	})?;

	let (misnamed_id, misnamed) = state_event(services, room_id, StateEvent {
		kind: "m.room.name",
		state_key: "",
		content: &json!({ "name": "other" }),
		auth_events: &[&other_id, &join_id],
	})?;

	let (unnamed_id, unnamed) = state_event(services, room_id, StateEvent {
		kind: "m.room.topic",
		state_key: "",
		content: &json!({ "topic": "unnamed" }),
		auth_events: &[&join_id],
	})?;

	admit(services, local, room_id, &named_id, named).await?;

	assert!(services.timeline.pdu_exists(&named_id).await);

	assert_forbidden(&admit(services, local, room_id, &other_id, other.clone()).await);
	assert!(!services.timeline.pdu_exists(&other_id).await, "another create is not stored");

	store(services, &StateEventType::RoomCreate, "", &other_id, other).await?;

	for (event_id, event) in [(misnamed_id, misnamed), (unnamed_id, unnamed)] {
		assert_forbidden(&admit(services, local, room_id, &event_id, event).await);
		assert!(!services.timeline.pdu_exists(&event_id).await, "{event_id} is not stored");
	}

	Ok(())
}

/// A stored outlier is not promoted to the timeline when it names another
/// create than the room's, or when it is a local user's join another user sent.
///
/// Each outlier stands for a copy stored before the checks existed: a name
/// citing a stored alternate create, and mallory's join for bob.
#[tokio::test]
async fn an_outlier_failing_provenance_is_not_promoted() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!promoted:localhost");
	let remote = server_name!("remote.example");
	let key = remote_key(services, remote)?;
	let [create_id, join_id] = existing_room(services, room_id).await?;
	let (other_id, other) = other_create(services, room_id)?;
	let (name_id, name) = state_event(services, room_id, StateEvent {
		kind: "m.room.name",
		state_key: "",
		content: &json!({ "name": "other" }),
		auth_events: &[&other_id, &join_id],
	})?;

	let forged = creator_join(room_id, "@bob:localhost", &create_id);
	let (forged_id, forged) = remote_signed(remote, &key, &RoomVersionId::V11, forged)?;

	store(services, &StateEventType::RoomCreate, "", &other_id, other).await?;
	store(services, &StateEventType::RoomName, "", &name_id, name.clone()).await?;

	let member = StateEventType::RoomMember;

	store(services, &member, "@bob:localhost", &forged_id, forged.clone()).await?;

	for (event_id, event) in [(name_id, name), (forged_id, forged)] {
		assert_forbidden(&promote(services, room_id, &create_id, &event_id, event).await);
		assert!(
			services
				.timeline
				.get_pdu_id(&event_id)
				.await
				.is_err(),
			"{event_id} stays an outlier"
		);
	}

	Ok(())
}

/// A remote user's join for this server's room creator is refused when pushed.
///
/// Bob created his room here and left it, and he stays left, where an admitted
/// forged join would rejoin him. The same join for the remote server's own
/// creator enters the timeline and joins that creator. Each join follows only
/// its room's create event, which the room version's rules allow before they
/// check that a join is self-sent.
#[tokio::test]
async fn a_forged_creator_join_is_refused_only_for_a_local_creator() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let remote = server_name!("remote.example");
	let key = remote_key(services, remote)?;
	let version = RoomVersionId::V11;
	let bob = user_id!("@bob:localhost");
	let local_room = room_id!("!local:localhost");
	let [(local_create, create), join] = room(services, local_room)?;
	let leave = state_event(services, local_room, StateEvent {
		kind: "m.room.member",
		state_key: bob.as_str(),
		content: &json!({ "membership": "leave" }),
		auth_events: &[&local_create, &join.0],
	})?;

	timeline_room(services, local_room, [(local_create.clone(), create), join, leave]).await?;

	assert!(
		services
			.state_cache
			.is_left(bob, local_room)
			.await
	);

	let forged = creator_join(local_room, bob.as_str(), &local_create);
	let forged = remote_signed(remote, &key, &version, forged)?;
	let forged_id = forged.0.clone();

	assert_forbidden(&receive(services, remote, local_room, forged).await);
	assert!(!services.timeline.pdu_exists(&forged_id).await, "the forged join is not stored");
	assert!(
		services
			.state_cache
			.is_left(bob, local_room)
			.await,
		"bob is still left"
	);

	let remote_bob = user_id!("@bob:remote.example");
	let remote_room = room_id!("!remote:remote.example");
	let create = json!({
		"type": "m.room.create",
		"state_key": "",
		"content": { "room_version": "11" },
		"room_id": remote_room,
		"sender": remote_bob,
		"origin_server_ts": 1,
		"depth": 1,
		"prev_events": [],
		"auth_events": [],
	});

	let create = remote_signed(remote, &key, &version, create)?;
	let create_id = create.0.clone();

	timeline_room(services, remote_room, [create]).await?;

	let join = creator_join(remote_room, remote_bob.as_str(), &create_id);
	let join = remote_signed(remote, &key, &version, join)?;
	let join_id = join.0.clone();

	receive(services, remote, remote_room, join).await?;

	let appended = services.timeline.get_pdu_id(&join_id).await;
	let joined = services
		.state_cache
		.is_joined(remote_bob, remote_room)
		.await;

	assert!(appended.is_ok(), "the remote creator's join enters the timeline");
	assert!(joined, "the remote creator joins");

	Ok(())
}

/// Appends a v11 room's events to its timeline in order, each with the state it
/// leaves, as for a room this server has followed from its creation.
///
/// The timeline lacks the empty state before the create event. Recording it
/// first lets an event whose only predecessor is the create resolve its state
/// here instead of fetching it.
async fn timeline_room<Events>(services: &Services, room_id: &RoomId, events: Events) -> Result
where
	Events: IntoIterator<Item = (OwnedEventId, Value)>,
{
	let rules = room_version_rules(&RoomVersionId::V11)?;
	let _shortroomid = services
		.short
		.get_or_create_shortroomid(room_id)
		.await;

	save_room_state(services, room_id, Arc::default()).await?;

	let state_lock = services.state.mutex.lock(room_id).await;

	for (event_id, event) in events {
		let event = from_value(event)?;
		let (pdu, event) = PduEvent::from_object_federation(room_id, &event_id, event, &rules)?;
		let statehash = services.state.append_to_state(&pdu).await?;

		services
			.timeline
			.append_pdu(&pdu, event, once(&*pdu.event_id), Some(statehash), &state_lock)
			.await?;
	}

	Ok(())
}

/// Stores a v11 room's create event and its creator's join as outliers, and
/// makes the two the room's current state.
///
/// The create event's ID comes first and the join's second.
async fn existing_room(services: &Services, room_id: &RoomId) -> Result<[OwnedEventId; 2]> {
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let create = store(services, &StateEventType::RoomCreate, "", &create_id, create).await?;
	let join =
		store(services, &StateEventType::RoomMember, "@bob:localhost", &join_id, join).await?;

	save_room_state(services, room_id, Arc::new([create, join].into())).await?;

	Ok([create_id, join_id])
}

/// Stores an event as an outlier with its event ID, and compresses its state
/// entry.
///
/// Auth-event reads expect the ID in the stored JSON.
async fn store(
	services: &Services,
	kind: &StateEventType,
	state_key: &str,
	event_id: &OwnedEventId,
	event: Value,
) -> Result<CompressedStateEvent> {
	let event = with_field(event, "event_id", json!(event_id));

	store_outlier(services, kind, state_key, event_id, event).await
}

/// Admits an event as an outlier of a v11 room.
///
/// Its auth events must already be held, since none are fetched.
async fn admit(
	services: &Services,
	origin: &ServerName,
	room_id: &RoomId,
	event_id: &EventId,
	event: Value,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let version = RoomVersionId::V11;

	services
		.event_handler
		.handle_outlier_pdu(origin, room_id, event_id, from_value(event)?, &version, 0, true)
		.await
}

/// Promotes a stored outlier of a v11 room against the room's create event.
///
/// Past the checks, promotion fetches state for the outlier and waits on the
/// network, so the call is bounded.
async fn promote(
	services: &Services,
	room_id: &RoomId,
	create_id: &EventId,
	event_id: &EventId,
	event: Value,
) -> Result<Handled> {
	let pdu: PduEvent = from_value(with_field(event.clone(), "event_id", json!(event_id)))?;
	let upgrade = PrevUpgrade {
		origin: server_name!("localhost"),
		room_id,
		event_id,
		room_version: &RoomVersionId::V11,
		recursion_level: 0,
		first_normal_ts: MilliSecondsSinceUnixEpoch::now(),
		create_event_id: create_id,
	};

	let promotion = services
		.event_handler
		.upgrade_outlier_to_timeline_pdu(upgrade, pdu, from_value(event)?);

	timeout(Duration::from_secs(10), promotion)
		.await
		.map_err(|_| err!("promotion did not stop at the checks"))?
}

/// Hands a timeline event to the federation receive path as another server's
/// push.
///
/// Past admission it can go on to fetch missing previous events from the
/// network, so the call is bounded.
async fn receive(
	services: &Services,
	origin: &ServerName,
	room_id: &RoomId,
	(event_id, event): (OwnedEventId, Value),
) -> Result<Handled> {
	let event = from_value(event)?;
	let is_timeline_event = true;
	let handled = services.event_handler.handle_incoming_pdu(
		origin,
		room_id,
		&event_id,
		event,
		is_timeline_event,
	);

	timeout(Duration::from_secs(10), handled)
		.await
		.map_err(|_| err!("the receive path did not stop at the checks"))?
}

#[track_caller]
fn assert_forbidden<T: Debug>(result: &Result<T>) {
	assert!(
		result
			.as_ref()
			.is_err_and(|error| matches!(error.kind(), ErrorKind::Forbidden)),
		"{result:?}"
	);
}
