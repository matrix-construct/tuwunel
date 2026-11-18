use std::{collections::HashMap, iter::once, slice::from_ref};

use ruma::{
	EventId, OwnedEventId, RoomId, RoomVersionId, UserId, events::StateEventType, room_id,
	server_name, user_id,
};
use serde_json::{Value, from_value, json, to_vec, value::to_raw_value};
use tuwunel_core::{Error, Result, config::Figment};
use tuwunel_matrix::{
	PduEvent,
	room::state::resolution::{AuthCheckOutcome, auth_check},
	room_version::rules as room_version_rules,
};

use super::super::SendJoinEvents;
use crate::{
	Services,
	rooms::state::IdMapState,
	test_utils::{
		StateEvent, creator_join, fixture, other_create, pdu_id, remote_key, remote_signed, room,
		signed, signed_event, signed_state, state_event, with_field,
	},
};

/// A send_join event is stored without its `unsigned` data, and redacted when
/// its content no longer matches its hash.
///
/// An event of another room in the same response is not stored.
#[tokio::test]
async fn an_altered_send_join_event_is_stored_redacted() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!join:localhost");
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let auth_events = [&create_id, &join_id];
	let topic = json!({"topic": "signed"});
	let name = json!({"name": "signed"});
	let (topic_id, topic_event) = state_event(services, room_id, StateEvent {
		kind: "m.room.topic",
		state_key: "",
		content: &topic,
		auth_events: &auth_events,
	})?;

	let (name_id, name_event) = state_event(services, room_id, StateEvent {
		kind: "m.room.name",
		state_key: "",
		content: &name,
		auth_events: &auth_events,
	})?;

	let (foreign_id, foreign) =
		signed_state(services, room_id!("!other:localhost"), "m.room.topic", "", &topic)?;

	let altered = with_field(topic_event, "content", json!({"topic": "altered"}));
	let annotated = with_field(name_event, "unsigned", json!({"age": 1}));

	ingest(services, room_id, &[create, join, altered, annotated, foreign]).await?;

	let altered: Value = services.timeline.get_outlier(&topic_id).await?;
	let annotated: Value = services.timeline.get_outlier(&name_id).await?;

	assert_eq!(altered["content"], json!({}), "an altered event is stored redacted");
	assert_eq!(annotated["content"], name);
	assert!(annotated.get("unsigned").is_none(), "remote unsigned data is dropped");
	assert!(!services.timeline.pdu_exists(&foreign_id).await);

	Ok(())
}

/// A send_join event leaves this server's copy alone unless that copy is an
/// outlier and the new one matches its content hash.
///
/// An accepted event gains no outlier beside it. An altered copy leaves a
/// stored outlier as it was. A verified copy replaces an outlier stored with
/// altered content.
#[tokio::test]
async fn a_send_join_event_replaces_only_an_altered_outlier() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!join:localhost");
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let auth_events = [&create_id, &join_id];
	let signed = |kind, content| {
		state_event(services, room_id, StateEvent {
			kind,
			state_key: "",
			content: &content,
			auth_events: &auth_events,
		})
	};

	let known_topic = json!({"topic": "known"});
	let genuine_avatar = json!({"url": "mxc://localhost/genuine"});
	let (accepted_id, accepted) = signed("m.room.name", json!({"name": "accepted"}))?;
	let (known_id, known) = signed("m.room.topic", known_topic.clone())?;
	let (forged_id, genuine) = signed("m.room.avatar", genuine_avatar.clone())?;
	let forged = with_field(genuine.clone(), "content", json!({"url": "mxc://localhost/forged"}));

	put_accepted(services, &accepted_id, &accepted)?;

	for (event_id, event) in [(&known_id, known.clone()), (&forged_id, forged)] {
		services
			.timeline
			.add_pdu_outlier(event_id, &from_value(event)?);
	}

	let altered = with_field(known, "content", json!({"topic": "altered"}));

	ingest(services, room_id, &[create, join, accepted, altered, genuine]).await?;

	let outlier_content = async |event_id| {
		services
			.timeline
			.get_outlier(event_id)
			.await
			.map(|event: Value| event["content"].clone())
	};

	let accepted_outlier = services
		.timeline
		.outlier_pdu_exists(&accepted_id)
		.await;

	assert!(accepted_outlier.is_err(), "an accepted event gains no outlier");
	assert_eq!(outlier_content(&known_id).await?, known_topic);
	assert_eq!(outlier_content(&forged_id).await?, genuine_avatar);

	Ok(())
}

/// A send_join response admits a room name even when its auth events come
/// later in the response.
///
/// Receiving the name on its own does not admit it to outlier storage.
#[tokio::test]
async fn a_send_join_event_needs_its_auth_chain() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!room:localhost");
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let auth = [&create_id, &join_id];
	let content = json!({"name": "Example"});
	let (name_id, name) = state_event(services, room_id, StateEvent {
		kind: "m.room.name",
		state_key: "",
		content: &content,
		auth_events: &auth,
	})?;

	ingest(services, room_id, from_ref(&name)).await?;

	assert!(!services.timeline.pdu_exists(&name_id).await);

	let state = ingest_response(services, room_id, &[], &[name, join, create]).await?;

	assert!(state.values().any(|id| id == &name_id));
	assert!(services.timeline.pdu_exists(&name_id).await);

	Ok(())
}

/// A send_join event the auth rules deny is dropped, and the rest of the
/// response is still admitted.
///
/// The name cites only the create event, so its sender's membership is absent
/// from the state it is checked against.
#[tokio::test]
async fn a_denied_send_join_event_is_dropped() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!denied:localhost");
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let unjoined = [&create_id];
	let joined = [&create_id, &join_id];
	let name = json!({"name": "denied"});
	let topic = json!({"topic": "allowed"});
	let (name_id, name) = state_event(services, room_id, StateEvent {
		kind: "m.room.name",
		state_key: "",
		content: &name,
		auth_events: &unjoined,
	})?;

	let (topic_id, topic) = state_event(services, room_id, StateEvent {
		kind: "m.room.topic",
		state_key: "",
		content: &topic,
		auth_events: &joined,
	})?;

	let state = ingest_response(services, room_id, &[], &[create, join, name, topic]).await?;

	assert!(!state.values().any(|id| id == &name_id), "a denied event is not state");
	assert!(!services.timeline.pdu_exists(&name_id).await, "a denied event is not stored");
	assert!(state.values().any(|id| id == &topic_id));

	Ok(())
}

/// A send_join response fails when an auth event it admitted cannot be read
/// back, rather than dropping the events that cite it.
///
/// The creator's join is held as an unreadable accepted row, which the
/// response's verified copy leaves in place. A readable outlier copy beside
/// that row is read instead, and the response is then admitted.
#[tokio::test]
async fn an_unreadable_auth_event_fails_the_send_join() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!unreadable:localhost");
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let name = json!({"name": "unread"});
	let (name_id, name) = state_event(services, room_id, StateEvent {
		kind: "m.room.name",
		state_key: "",
		content: &name,
		auth_events: &[&create_id, &join_id],
	})?;

	let response = [create, join.clone(), name];

	put_accepted(services, &join_id, &json!("unreadable"))?;

	let result = ingest_response(services, room_id, &[], &response).await;

	assert!(
		matches!(result, Err(Error::SerdeDe(_))),
		"an unreadable auth event fails the response with its read error"
	);

	assert!(!services.timeline.pdu_exists(&name_id).await);

	let stored_join = with_field(join, "event_id", json!(join_id));

	services
		.timeline
		.add_pdu_outlier(&join_id, &from_value(stored_join)?);

	let state = ingest_response(services, room_id, &[], &response).await?;

	assert!(state.values().any(|id| id == &name_id), "a readable copy is read instead");

	Ok(())
}

/// A create event naming auth events is denied by its own rule, without
/// reading them, and fails the send_join response.
///
/// The event names no previous events, so the auth-events rule decides.
#[tokio::test]
async fn a_create_event_naming_auth_events_fails_the_send_join() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!create:localhost");
	let version = RoomVersionId::V11;
	let absent = EventId::parse("$absent")?;
	let naming = StateEvent {
		kind: "m.room.create",
		state_key: "",
		content: &json!({ "room_version": "11" }),
		auth_events: &[&absent],
	};

	let (create_id, create) = signed_event(services, room_id, naming, &[])?;

	let pdu: PduEvent = from_value(with_field(create.clone(), "event_id", json!(create_id)))?;
	let outcome = services
		.event_handler
		.auth_check_by_auth_events(&pdu, &room_version_rules(&version)?)
		.await?;

	assert!(matches!(outcome, AuthCheckOutcome::Deny(_)), "the create rule denies it");

	let refused = ingest_response(services, room_id, &[], &[create]).await;

	assert!(matches!(refused, Err(Error::BadServerResponse(_))), "the response fails");

	Ok(())
}

/// A send_join `state` list naming two events for one state key fails the
/// response.
///
/// One event listed twice, and again in the auth chain, is state once.
#[tokio::test]
async fn two_send_join_events_for_one_state_key_fail_the_response() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!conflict:localhost");
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let auth = [&create_id, &join_id];
	let signed = |content| {
		state_event(services, room_id, StateEvent {
			kind: "m.room.name",
			state_key: "",
			content: &content,
			auth_events: &auth,
		})
	};

	let (first_id, first) = signed(json!({"name": "first"}))?;
	let (_, second) = signed(json!({"name": "second"}))?;
	let room_events = [create, join];
	let auth_chain = [room_events.as_slice(), from_ref(&first)].concat();
	let repeated = [auth_chain.as_slice(), from_ref(&first)].concat();
	let conflicting = [room_events.as_slice(), &[first, second]].concat();

	let state = ingest_response(services, room_id, &auth_chain, &repeated).await?;

	assert_eq!(
		state
			.values()
			.filter(|&id| id.eq(&first_id))
			.count(),
		1
	);

	let refused = ingest_response(services, room_id, &[], &conflicting).await;

	assert!(
		matches!(refused, Err(Error::BadServerResponse(_))),
		"a second event for one state key fails the response"
	);

	Ok(())
}

/// A send_join event admitted with its content redacted is stored whole when a
/// verified copy follows in the same response.
///
/// The redacted copy comes first, in the auth chain, so the verified one in the
/// `state` list repeats an admitted event.
#[tokio::test]
async fn a_later_verified_copy_replaces_a_redacted_send_join_event() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!upgrade:localhost");
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let topic = json!({"topic": "genuine"});
	let (topic_id, genuine) = state_event(services, room_id, StateEvent {
		kind: "m.room.topic",
		state_key: "",
		content: &topic,
		auth_events: &[&create_id, &join_id],
	})?;

	let altered = with_field(genuine.clone(), "content", json!({"topic": "altered"}));
	let state = ingest_response(services, room_id, &[create, join, altered], &[genuine]).await?;
	let stored: Value = services.timeline.get_outlier(&topic_id).await?;

	assert_eq!(stored["content"], topic, "the verified copy replaces the redacted one");
	assert!(state.values().any(|id| id == &topic_id));

	Ok(())
}

/// A repeated event is checked again once an auth event's stored copy was
/// replaced by its verified copy.
///
/// In this room version 10 response the power levels first arrive altered, so
/// redaction strips their invite level and dave's invite of carol passes. The
/// verified copy in the `state` list restores that level, and the invite's copy
/// there is denied and stays out of the state.
#[tokio::test]
async fn a_repeated_event_is_checked_again_after_a_stored_copy_is_replaced() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let version = RoomVersionId::V10;
	let room_id = room_id!("!replaced:localhost");
	let event = |kind, state_key, sender, content, auth: &[&OwnedEventId]| {
		let prev_events: Vec<_> = auth.last().into_iter().collect();
		let event = json!({
			"type": kind,
			"state_key": state_key,
			"content": content,
			"room_id": room_id,
			"sender": sender,
			"origin_server_ts": 1,
			"depth": auth.len().saturating_add(1),
			"prev_events": prev_events,
			"auth_events": auth,
		});

		signed(services, &version, event)
	};

	let bob = "@bob:localhost";
	let dave = "@dave:localhost";
	let create_content = json!({ "creator": bob, "room_version": "10" });
	let (create_id, create) = event("m.room.create", "", bob, create_content, &[])?;
	let joined = json!({ "membership": "join" });
	let (bob_id, bob_join) = event("m.room.member", bob, bob, joined.clone(), &[&create_id])?;
	let public = json!({ "join_rule": "public" });
	let room_auth = [&create_id, &bob_id];
	let (rules_id, rules) = event("m.room.join_rules", "", bob, public, &room_auth)?;
	let levels = json!({ "users": { bob: 100 }, "invite": 50 });
	let (levels_id, levels) = event("m.room.power_levels", "", bob, levels, &room_auth)?;
	let altered = with_field(levels.clone(), "content", json!({ "users": { bob: 100 } }));
	let join_auth = [&create_id, &rules_id, &levels_id];
	let (dave_id, dave_join) = event("m.room.member", dave, dave, joined, &join_auth)?;
	let invited = json!({ "membership": "invite" });
	let invite_auth = [&create_id, &rules_id, &levels_id, &dave_id];
	let carol = "@carol:localhost";
	let (invite_id, invite) = event("m.room.member", carol, dave, invited, &invite_auth)?;
	let room = [create, bob_join, rules];
	let auth_chain = [room.as_slice(), &[altered, dave_join.clone(), invite.clone()]].concat();
	let listed = [room.as_slice(), &[levels, dave_join, invite]].concat();

	let state =
		ingest_as(services, room_id, user_id!("@dave:localhost"), &version, &auth_chain, &listed)
			.await?;

	assert!(state.values().any(|id| id == &levels_id));
	assert!(state.values().any(|id| id == &dave_id));
	assert!(services.timeline.pdu_exists(&invite_id).await, "its first copy passed");
	assert!(!state.values().any(|id| id == &invite_id), "the invite is checked again");

	Ok(())
}

/// A room version 12 response admits its events however they are ordered, each
/// waiting for the create event the room ID implies.
///
/// The creator's join names no auth events, so the implied create is all it
/// waits for. Received alone, the name is not admitted.
#[tokio::test]
async fn a_v12_send_join_event_waits_for_the_implied_create() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let version = RoomVersionId::V12;
	let create = json!({
		"type": "m.room.create",
		"state_key": "",
		"content": { "room_version": "12" },
		"sender": "@bob:localhost",
		"origin_server_ts": 1,
		"depth": 1,
		"prev_events": [],
		"auth_events": [],
	});

	let (create_id, create) = signed(services, &version, create)?;

	let room_id = RoomId::parse(create_id.as_str().replacen('$', "!", 1))?;
	let join = json!({
		"type": "m.room.member",
		"state_key": "@bob:localhost",
		"content": { "membership": "join" },
		"room_id": room_id,
		"sender": "@bob:localhost",
		"origin_server_ts": 1,
		"depth": 2,
		"prev_events": [create_id],
		"auth_events": [],
	});

	let (join_id, join) = signed(services, &version, join)?;

	let name = json!({
		"type": "m.room.name",
		"state_key": "",
		"content": { "name": "hydra" },
		"room_id": room_id,
		"sender": "@bob:localhost",
		"origin_server_ts": 1,
		"depth": 3,
		"prev_events": [join_id],
		"auth_events": [join_id],
	});

	let (name_id, name) = signed(services, &version, name)?;

	let bob = user_id!("@bob:localhost");

	ingest_as(services, &room_id, bob, &version, &[], from_ref(&name)).await?;

	assert!(!services.timeline.pdu_exists(&name_id).await);

	let state = ingest_as(services, &room_id, bob, &version, &[], &[name, join, create]).await?;

	assert!(state.values().any(|id| id == &name_id));
	assert!(state.values().any(|id| id == &create_id));

	Ok(())
}

#[tokio::test]
async fn a_create_event_of_another_room_version_fails_the_send_join() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!version:localhost");
	let (_, create) = state_event(services, room_id, StateEvent {
		kind: "m.room.create",
		state_key: "",
		content: &json!({ "room_version": "10" }),
		auth_events: &[],
	})?;

	let refused = ingest_response(services, room_id, &[], &[create]).await;

	assert!(matches!(refused, Err(Error::BadServerResponse(_))), "the response fails");

	Ok(())
}

/// An event citing a denied auth event is dropped with it, and the topic beside
/// them is still admitted.
///
/// Bob sends a join for carol, a user of another server, which the rules deny,
/// then invites carol citing that join as her membership.
#[tokio::test]
async fn an_event_citing_a_denied_send_join_event_is_dropped() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!cascade:localhost");
	let carol = "@carol:remote.example";
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let (forged_id, forged) = state_event(services, room_id, StateEvent {
		kind: "m.room.member",
		state_key: carol,
		content: &json!({ "membership": "join" }),
		auth_events: &[&create_id, &join_id],
	})?;

	let (invite_id, invite) = state_event(services, room_id, StateEvent {
		kind: "m.room.member",
		state_key: carol,
		content: &json!({ "membership": "invite" }),
		auth_events: &[&create_id, &join_id, &forged_id],
	})?;

	let (topic_id, topic) = state_event(services, room_id, StateEvent {
		kind: "m.room.topic",
		state_key: "",
		content: &json!({ "topic": "kept" }),
		auth_events: &[&create_id, &join_id],
	})?;

	let response = [create, join, forged, invite, topic];
	let state = ingest_response(services, room_id, &[], &response).await?;

	assert!(!services.timeline.pdu_exists(&forged_id).await, "the denied join is dropped");
	assert!(!services.timeline.pdu_exists(&invite_id).await, "its dependent is dropped");
	assert!(state.values().any(|id| id == &topic_id));

	Ok(())
}

/// A join for a local user that another user sent is dropped from either list
/// of a send_join response, whether or not the room already has state here.
///
/// Mallory's joins for bob each follow only the create event, which the room
/// version's rules allow, so only the provenance check refuses them.
#[tokio::test]
async fn a_forged_local_join_is_dropped_from_either_list() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let remote = server_name!("remote.example");
	let key = remote_key(services, remote)?;
	let version = RoomVersionId::V11;
	let rooms = [(room_id!("!fresh:localhost"), false), (room_id!("!held:localhost"), true)];

	for (room_id, held) in rooms {
		let [(create_id, create), (join_id, join)] = room(services, room_id)?;

		if held {
			install_initial_join_state(services, room_id, &[create.clone(), join.clone()])
				.await?;
		}

		let listed = creator_join(room_id, "@bob:localhost", &create_id);
		let (listed_id, listed) = remote_signed(remote, &key, &version, listed)?;
		let content = json!({ "membership": "join", "displayname": "stated" });
		let stated = creator_join(room_id, "@bob:localhost", &create_id);
		let stated = with_field(stated, "content", content);
		let (stated_id, stated) = remote_signed(remote, &key, &version, stated)?;
		let auth_chain = [create.clone(), join, listed];
		let state = ingest_response(services, room_id, &auth_chain, &[create, stated]).await?;

		assert!(!services.timeline.pdu_exists(&listed_id).await, "{room_id}: from auth_chain");
		assert!(!services.timeline.pdu_exists(&stated_id).await, "{room_id}: from state");
		assert!(services.timeline.pdu_exists(&join_id).await, "{room_id}: bob's own join");
		assert!(state.values().any(|id| id == &create_id), "{room_id}: the create is state");
	}

	Ok(())
}

async fn install_initial_join_state(
	services: &Services,
	room_id: &RoomId,
	state: &[Value],
) -> Result {
	let held = ingest_response(services, room_id, &[], state).await?;
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.membership
		.apply_send_join_state(room_id, &held, &state_lock)
		.await
}

/// A send_join response drops a forged local join and an event the rules deny,
/// with the events citing each, and the joining user's join still completes.
///
/// Mallory's join for bob follows only the create event, which the rules allow,
/// so only the provenance check refuses it; bob's join for carol, a user of
/// another server, passes that check and the rules deny it. Bob's topic names
/// the first as his membership and his invite of carol names the second as
/// hers. Alice then joins the public room against the admitted state.
#[tokio::test]
async fn forged_and_denied_events_are_dropped_and_the_join_completes() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!completes:localhost");
	let remote = server_name!("remote.example");
	let key = remote_key(services, remote)?;
	let version = RoomVersionId::V11;
	let carol = "@carol:remote.example";
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let (rules_id, rules) = state_event(services, room_id, StateEvent {
		kind: "m.room.join_rules",
		state_key: "",
		content: &json!({ "join_rule": "public" }),
		auth_events: &[&create_id, &join_id],
	})?;

	let forged = creator_join(room_id, "@bob:localhost", &create_id);
	let (forged_id, forged) = remote_signed(remote, &key, &version, forged)?;
	let (topic_id, topic) = state_event(services, room_id, StateEvent {
		kind: "m.room.topic",
		state_key: "",
		content: &json!({ "topic": "forged" }),
		auth_events: &[&create_id, &forged_id],
	})?;

	let (denied_id, denied) = state_event(services, room_id, StateEvent {
		kind: "m.room.member",
		state_key: carol,
		content: &json!({ "membership": "join" }),
		auth_events: &[&create_id, &join_id],
	})?;

	let (invite_id, invite) = state_event(services, room_id, StateEvent {
		kind: "m.room.member",
		state_key: carol,
		content: &json!({ "membership": "invite" }),
		auth_events: &[&create_id, &join_id, &denied_id],
	})?;

	let auth_chain = [forged, denied];
	let response = [create, join, rules, topic, invite];
	let state = ingest_response(services, room_id, &auth_chain, &response).await?;

	for (event_id, what) in [
		(&forged_id, "the forged join"),
		(&topic_id, "its dependent"),
		(&denied_id, "the denied join"),
		(&invite_id, "its dependent"),
	] {
		assert!(!services.timeline.pdu_exists(event_id).await, "{what} is dropped");
	}

	let alice = user_id!("@alice:localhost");
	let alice_join = json!({
		"type": "m.room.member",
		"state_key": alice,
		"content": { "membership": "join" },
		"room_id": room_id,
		"sender": alice,
		"origin_server_ts": 1,
		"depth": 3,
		"prev_events": [rules_id],
		"auth_events": [create_id, rules_id],
	});

	let alice_join = signed(services, &version, alice_join)?;

	complete_join(services, room_id, &state, alice_join).await?;

	let joined = services
		.state_cache
		.is_joined(alice, room_id)
		.await;

	assert!(joined, "alice joins");

	Ok(())
}

/// Completes a v11 remote join as `join_remote` does once the response is in.
///
/// `state` is what the response admitted, and the joining event must pass the
/// rules against it.
async fn complete_join(
	services: &Services,
	room_id: &RoomId,
	state: &HashMap<u64, OwnedEventId>,
	(event_id, event): (OwnedEventId, Value),
) -> Result {
	let rules = room_version_rules(&RoomVersionId::V11)?;
	let event = from_value(event)?;
	let (pdu, event) = PduEvent::from_object_federation(room_id, &event_id, event, &rules)?;
	let ids = IdMapState { services, ids: state };

	auth_check(&rules, &pdu, &*services.timeline, ids)
		.await?
		.into_result()?;

	let _shortroomid = services
		.short
		.get_or_create_shortroomid(room_id)
		.await;

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.membership
		.apply_send_join_state(room_id, state, &state_lock)
		.await?;

	let statehash = services.state.append_to_state(&pdu).await?;

	services
		.timeline
		.append_pdu(&pdu, event, once(&*pdu.event_id), Some(statehash), &state_lock)
		.await?;

	Ok(())
}

/// A send_join response for a room this server holds drops another create
/// event from either list, and the events naming it, and admits the rest.
///
/// Once the room's state pointer cannot be read, the response fails rather than
/// being checked against no create.
#[tokio::test]
async fn a_send_join_event_must_name_the_known_create() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!anchored:localhost");
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;

	install_initial_join_state(services, room_id, &[create.clone(), join.clone()]).await?;

	let (other_id, other) = other_create(services, room_id)?;
	let (renamed_id, renamed) = state_event(services, room_id, StateEvent {
		kind: "m.room.name",
		state_key: "",
		content: &json!({ "name": "other" }),
		auth_events: &[&other_id, &join_id],
	})?;

	let (topic_id, topic) = state_event(services, room_id, StateEvent {
		kind: "m.room.topic",
		state_key: "",
		content: &json!({ "topic": "anchored" }),
		auth_events: &[&create_id, &join_id],
	})?;

	let state = [create.clone(), join.clone(), renamed, topic];
	let admitted = ingest_response(services, room_id, from_ref(&other), &state).await?;

	assert!(!services.timeline.pdu_exists(&other_id).await, "from auth_chain");
	assert!(!services.timeline.pdu_exists(&renamed_id).await, "an event naming it");
	assert!(admitted.values().any(|id| id == &topic_id));

	let admitted = ingest_response(services, room_id, &[], &[create, join, other]).await?;

	assert!(!services.timeline.pdu_exists(&other_id).await, "from state");
	assert!(admitted.values().any(|id| id == &create_id));

	services.db["roomid_shortstatehash"].insert(room_id.as_bytes(), b"bad");

	let failed = ingest_response(services, room_id, &[], &state).await;

	assert!(failed.is_err(), "an unreadable state pointer fails the response");

	Ok(())
}

/// A send_join response's member event for another local user replaces a held
/// departure only when it follows that departure in its auth events.
///
/// A first response leaves alice's own leave as her current membership.
/// Omitting her or sending a ban that does not cite her leave keeps it in
/// place; a ban that follows the leave replaces it.
#[tokio::test]
async fn a_send_join_member_event_installs_over_a_local_departure() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!departed:localhost");
	let alice = user_id!("@alice:localhost");
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let (rules_id, rules) = state_event(services, room_id, StateEvent {
		kind: "m.room.join_rules",
		state_key: "",
		content: &json!({ "join_rule": "public" }),
		auth_events: &[&create_id, &join_id],
	})?;

	let membership = |content, auth: &[&OwnedEventId]| {
		let prev_events: Vec<_> = auth.last().into_iter().collect();
		let event = json!({
			"type": "m.room.member",
			"state_key": alice,
			"content": content,
			"room_id": room_id,
			"sender": alice,
			"origin_server_ts": 1,
			"depth": auth.len().saturating_add(1),
			"prev_events": prev_events,
			"auth_events": auth,
		});

		signed(services, &RoomVersionId::V11, event)
	};

	let joined = json!({ "membership": "join" });
	let (alice_join_id, alice_join) = membership(joined, &[&create_id, &rules_id])?;
	let left = json!({ "membership": "leave" });
	let (leave_id, leave) = membership(left, &[&create_id, &alice_join_id])?;
	let (_, unanchored_ban) = state_event(services, room_id, StateEvent {
		kind: "m.room.member",
		state_key: alice.as_str(),
		content: &json!({ "membership": "ban" }),
		auth_events: &[&create_id, &join_id],
	})?;

	let (ban_id, ban) = state_event(services, room_id, StateEvent {
		kind: "m.room.member",
		state_key: alice.as_str(),
		content: &json!({ "membership": "ban" }),
		auth_events: &[&create_id, &join_id, &leave_id],
	})?;

	let _shortroomid = services
		.short
		.get_or_create_shortroomid(room_id)
		.await;

	let current = async || {
		services
			.state_accessor
			.room_state_get(room_id, &StateEventType::RoomMember, alice.as_str())
			.await
			.map(|event| event.event_id)
	};

	let departed = [create.clone(), join.clone(), rules.clone(), leave.clone()];
	let held = ingest_response(services, room_id, from_ref(&alice_join), &departed).await?;
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.membership
		.apply_send_join_state(room_id, &held, &state_lock)
		.await?;

	assert_eq!(current().await?, leave_id, "alice's leave is her current membership");
	assert!(services.state_cache.is_left(alice, room_id).await);

	for (label, response) in [
		("omitted", vec![create.clone(), join.clone()]),
		("unanchored ban", vec![create.clone(), join.clone(), unanchored_ban.clone()]),
	] {
		let state = ingest_response(services, room_id, &[], &response).await?;
		services
			.membership
			.apply_send_join_state(room_id, &state, &state_lock)
			.await?;
		assert_eq!(current().await?, leave_id, "{label} keeps alice's leave");
	}

	let auth_chain = [rules, alice_join, leave];
	let conflict = [create.clone(), join.clone(), unanchored_ban, ban.clone()];
	let refused = ingest_response(services, room_id, &auth_chain, &conflict).await;
	assert!(matches!(refused, Err(Error::BadServerResponse(_))));

	let state = ingest_response(services, room_id, &auth_chain, &[create, join, ban]).await?;
	services
		.membership
		.apply_send_join_state(room_id, &state, &state_lock)
		.await?;
	assert_eq!(current().await?, ban_id, "a ban following the leave replaces it");

	Ok(())
}

#[tokio::test]
async fn two_send_join_member_events_for_another_local_user_fail_the_response() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!member-conflict:localhost");
	let alice = user_id!("@alice:localhost");
	let [(create_id, create), (join_id, join)] = room(services, room_id)?;
	let (rules_id, rules) = state_event(services, room_id, StateEvent {
		kind: "m.room.join_rules",
		state_key: "",
		content: &json!({ "join_rule": "public" }),
		auth_events: &[&create_id, &join_id],
	})?;
	let (alice_join_id, alice_join) = signed(
		services,
		&RoomVersionId::V11,
		json!({
			"type": "m.room.member", "state_key": alice, "sender": alice, "room_id": room_id,
			"content": { "membership": "join" }, "origin_server_ts": 1, "depth": 3,
			"prev_events": [&rules_id], "auth_events": [&create_id, &rules_id],
		}),
	)?;
	let (_, ban) = state_event(services, room_id, StateEvent {
		kind: "m.room.member",
		state_key: alice.as_str(),
		content: &json!({ "membership": "ban" }),
		auth_events: &[&create_id, &join_id, &alice_join_id],
	})?;

	let repeated = [create.clone(), join.clone(), alice_join.clone(), alice_join.clone()];
	ingest_response(services, room_id, from_ref(&rules), &repeated).await?;
	let refused =
		ingest_response(services, room_id, from_ref(&rules), &[create, join, alice_join, ban])
			.await;
	assert!(matches!(refused, Err(Error::BadServerResponse(_))));

	Ok(())
}

async fn ingest(services: &Services, room_id: &RoomId, events: &[Value]) -> Result {
	ingest_response(services, room_id, events, &[]).await?;

	Ok(())
}

/// Ingests a v11 send_join response's `auth_chain` and `state` lists, returning
/// the state it names.
///
/// Only events of the `state` list enter that state; an event listed only in
/// the auth chain is stored without becoming state.
async fn ingest_response(
	services: &Services,
	room_id: &RoomId,
	auth_chain: &[Value],
	state: &[Value],
) -> Result<HashMap<u64, OwnedEventId>> {
	let bob = user_id!("@bob:localhost");
	ingest_as(services, room_id, bob, &RoomVersionId::V11, auth_chain, state).await
}

async fn ingest_as(
	services: &Services,
	room_id: &RoomId,
	joining: &UserId,
	version: &RoomVersionId,
	auth_chain: &[Value],
	state: &[Value],
) -> Result<HashMap<u64, OwnedEventId>> {
	let raw = |events: &[Value]| {
		events
			.iter()
			.map(to_raw_value)
			.collect::<Result<Vec<_>, _>>()
	};

	let auth_chain = raw(auth_chain)?;
	let state = raw(state)?;
	let events = SendJoinEvents { auth_chain: &auth_chain, state: &state };

	services
		.membership
		.ingest_send_join_events(room_id, joining, version, &room_version_rules(version)?, events)
		.await
}

/// Writes an accepted copy of an event straight to storage.
///
/// The event's ID maps to a timeline row holding it, so it reads as accepted,
/// and no outlier row is written.
fn put_accepted(services: &Services, event_id: &EventId, event: &Value) -> Result {
	let pdu_id = pdu_id(1);

	services.db["eventid_pduid"].insert(event_id.as_bytes(), pdu_id.as_bytes());
	services.db["pduid_pdu"].insert(pdu_id.as_bytes(), to_vec(event)?);

	Ok(())
}
