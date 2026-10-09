use std::{
	str::FromStr,
	sync::Arc,
	time::{Duration, UNIX_EPOCH},
};

use futures::future::join;
use ruma::{
	EventId, RoomId, UInt, UserId,
	api::federation::space::SpaceHierarchyParentSummary,
	event_id,
	events::{
		StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	owned_room_id, owned_server_name,
	room::{JoinRuleSummary, RestrictedSummary, RoomSummary},
	room_id, server_name, user_id,
};
use serde_json::{Value, json};
use tokio::{sync::Notify, task_local, time::timeout};
use tuwunel_core::{Err, PduCount, Result, config::Figment, err, utils::time::now};
use tuwunel_database::Json;

use super::{Accessibility, Identifier, PaginationToken, get_parent_children_via};
use crate::{
	Services,
	rooms::{
		spaces::{
			cache::{Cached, Provenance},
			federation::summary_matches_room,
		},
		state_cache::MembershipUpdate,
	},
	test_utils::fixture,
};

struct AccessControl {
	reached: Notify,
	release: Notify,
}

task_local! {
	static ACCESS_CONTROL: Arc<AccessControl>;
	static REMOTE_CONTROL: Arc<AccessControl>;
}

pub(super) async fn after_cached_access(accessible: bool) -> bool {
	if let Ok(control) = ACCESS_CONTROL.try_with(Arc::clone) {
		control.reached.notify_one();
		control.release.notified().await;
	}

	accessible
}

pub(super) async fn after_remote_room(remote: bool) -> bool {
	if let Ok(control) = REMOTE_CONTROL.try_with(Arc::clone) {
		control.reached.notify_one();
		control.release.notified().await;
	}

	remote
}

#[tokio::test]
async fn cached_access_rechecks_membership_after_await() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!space:localhost");
	let user = user_id!("@member:localhost");
	let mut summary = parent_summary(room.as_str());

	summary.summary.name = Some("remote".into());
	seed_local_summary(services, room, user).await?;

	services
		.spaces
		.cache_put(room, Some(&summary), Provenance::Remote);

	let control = Arc::new(AccessControl {
		reached: Notify::new(),
		release: Notify::new(),
	});

	let identifier = Identifier::UserId(user);

	let request = ACCESS_CONTROL.scope(
		control.clone(),
		services
			.spaces
			.get_summary_and_children_local(room, &identifier),
	);

	let membership = controlled_transition(&control, services, room, user);

	let (result, membership) = timeout(Duration::from_secs(10), join(request, membership))
		.await
		.map_err(|_| err!("cached access schedule timed out"))?;

	membership?;
	let Accessibility::Accessible(local) = result? else {
		return Err!("joined local summary was inaccessible");
	};

	assert_eq!(local.summary.name.as_deref(), Some("local"));
	assert_eq!(local.summary.join_rule, JoinRuleSummary::Invite);

	Ok(())
}

#[tokio::test]
async fn late_remote_write_is_unusable_after_join() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!child:localhost");
	let user = user_id!("@member:localhost");
	let mut summary = parent_summary(room.as_str());

	summary.summary.name = Some("remote".into());
	seed_local_summary(services, room, user).await?;

	let control = Arc::new(AccessControl {
		reached: Notify::new(),
		release: Notify::new(),
	});

	let eligibility = REMOTE_CONTROL.scope(control.clone(), services.spaces.remote_room(room));
	let membership = controlled_transition(&control, services, room, user);

	let (remote, membership) = timeout(Duration::from_secs(10), join(eligibility, membership))
		.await
		.map_err(|_| err!("late write schedule timed out"))?;

	membership?;
	assert!(remote?);
	services
		.spaces
		.cache_put(room, Some(&summary), Provenance::Remote);

	let result = services
		.spaces
		.get_summary_and_children_local(room, &Identifier::UserId(user))
		.await?;

	let Accessibility::Accessible(local) = result else {
		return Err!("joined local summary was inaccessible");
	};

	assert_eq!(local.summary.name.as_deref(), Some("local"));
	assert_eq!(local.summary.join_rule, JoinRuleSummary::Invite);

	Ok(())
}

async fn transition(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	membership: MembershipState,
) -> Result {
	services
		.state_cache
		.update_membership(MembershipUpdate {
			room_id,
			user_id,
			membership_event: RoomMemberEventContent::new(membership),
			sender: user_id,
			last_state: None,
			invite_via: None,
			update_joined_count: true,
			count: PduCount::Normal(*services.globals.next_count()),
		})
		.await
}

async fn controlled_transition(
	control: &AccessControl,
	services: &Services,
	room: &RoomId,
	user: &UserId,
) -> Result {
	control.reached.notified().await;
	transition(services, room, user, MembershipState::Join).await?;
	control.release.notify_one();

	Ok(())
}

async fn seed_local_summary(services: &Services, room: &RoomId, sender: &UserId) -> Result {
	let name_id = event_id!("$local-name:localhost");
	let rules_id = event_id!("$local-rules:localhost");
	let acl_id = event_id!("$local-acl:localhost");
	let acl = json!({
		"allow": ["*"],
		"deny": ["blocked.example"],
		"allow_ip_literals": false,
	});
	let events = [
		(StateEventType::RoomName, name_id, json!({"name": "local"})),
		(StateEventType::RoomJoinRules, rules_id, json!({"join_rule": "invite"})),
		(StateEventType::RoomServerAcl, acl_id, acl),
	];

	let mut compressed = Vec::with_capacity(events.len());

	for (event_type, event_id, content) in events {
		services.db["eventid_outlierpdu"]
			.raw_put(event_id, Json(state_event(event_id, room, sender, &event_type, &content)));

		let key = services
			.short
			.get_or_create_shortstatekey(&event_type, "")
			.await;

		compressed.push(
			services
				.state_compressor
				.compress_state_event(key, event_id)
				.await,
		);
	}

	let state = Arc::new(compressed.into_iter().collect());
	let hash = services
		.state
		.set_event_state(name_id, room, state)
		.await?;

	let lock = services.state.mutex.lock(room).await;

	services.state.set_room_state(room, hash, &lock);
	Ok(())
}

fn state_event(
	event_id: &EventId,
	room_id: &RoomId,
	sender: &UserId,
	event_type: &StateEventType,
	content: &Value,
) -> Value {
	state_event_with_key(event_id, room_id, sender, event_type, "", content)
}

fn state_event_with_key(
	event_id: &EventId,
	room_id: &RoomId,
	sender: &UserId,
	event_type: &StateEventType,
	state_key: &str,
	content: &Value,
) -> Value {
	json!({
		"event_id": event_id, "room_id": room_id, "sender": sender,
		"type": event_type, "state_key": state_key, "content": content,
		"origin": "localhost", "origin_server_ts": 1, "depth": 1,
		"prev_events": [], "auth_events": [], "signatures": {},
		"hashes": {"sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"},
	})
}

#[tokio::test]
async fn legacy_cache_respects_local_authority() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!legacy:localhost");
	let user = user_id!("@member:localhost");
	let mut summary = parent_summary(room.as_str());

	summary.summary.name = Some("legacy".into());
	seed_local_summary(services, room, user).await?;
	transition(services, room, user, MembershipState::Join).await?;
	seed_legacy_cache(services, room, &summary);

	let result = services
		.spaces
		.get_summary_and_children_local(room, &Identifier::UserId(user))
		.await?;

	let Accessibility::Accessible(local) = result else {
		return Err!("joined local summary was inaccessible");
	};

	assert_eq!(local.summary.name.as_deref(), Some("local"));
	assert_eq!(local.summary.join_rule, JoinRuleSummary::Invite);

	transition(services, room, user, MembershipState::Leave).await?;
	seed_legacy_cache(services, room, &summary);

	let result = services
		.spaces
		.get_summary_and_children_local(room, &Identifier::UserId(user))
		.await;

	assert!(result.is_err_and(|error| error.is_not_found()));

	Ok(())
}

#[tokio::test]
async fn wrong_room_remote_cache_is_rejected() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!requested:localhost");
	let user = user_id!("@member:localhost");
	let summary = parent_summary("!other:localhost");

	services
		.spaces
		.cache_put(room, Some(&summary), Provenance::Remote);

	let result = services
		.spaces
		.get_summary_and_children_local(room, &Identifier::UserId(user))
		.await;

	assert!(result.is_err_and(|error| error.is_not_found()));

	Ok(())
}

#[tokio::test]
async fn local_cache_hit_returns_cached_summary() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!cached:localhost");
	let user = user_id!("@member:localhost");
	let mut summary = parent_summary(room.as_str());

	summary.summary.name = Some("sentinel".into());
	transition(services, room, user, MembershipState::Join).await?;
	services
		.spaces
		.cache_put(room, Some(&summary), Provenance::Local);

	let result = services
		.spaces
		.get_summary_and_children_local(room, &Identifier::UserId(user))
		.await?;

	let Accessibility::Accessible(cached) = result else {
		return Err!("local cache hit was inaccessible");
	};

	assert_eq!(cached.summary.name.as_deref(), Some("sentinel"));

	Ok(())
}

#[tokio::test]
async fn state_install_evicts_local_cache() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!eviction:localhost");
	let user = user_id!("@member:localhost");
	let mut sentinel = parent_summary(room.as_str());

	sentinel.summary.name = Some("sentinel".into());
	seed_local_summary(services, room, user).await?;
	transition(services, room, user, MembershipState::Join).await?;
	services
		.spaces
		.cache_put(room, Some(&sentinel), Provenance::Local);

	install_changed_summary(services, room, user).await?;

	let result = services
		.spaces
		.get_summary_and_children_local(room, &Identifier::UserId(user))
		.await?;

	let Accessibility::Accessible(summary) = result else {
		return Err!("changed local summary was inaccessible");
	};

	assert_eq!(summary.summary.name.as_deref(), Some("changed"));
	assert_eq!(summary.summary.join_rule, JoinRuleSummary::Public);
	assert_eq!(summary.children_state.len(), 1);

	Ok(())
}

async fn install_changed_summary(services: &Services, room: &RoomId, sender: &UserId) -> Result {
	let name_id = event_id!("$changed-name:localhost");
	let rules_id = event_id!("$changed-rules:localhost");
	let child_id = event_id!("$changed-child:localhost");
	let events = [
		(StateEventType::RoomName, name_id, "", json!({"name": "changed"})),
		(StateEventType::RoomJoinRules, rules_id, "", json!({"join_rule": "public"})),
		(
			StateEventType::SpaceChild,
			child_id,
			"!nested:localhost",
			json!({"via": ["localhost"]}),
		),
	];

	let mut compressed = Vec::with_capacity(events.len());

	for (event_type, event_id, state_key, content) in events {
		let event =
			state_event_with_key(event_id, room, sender, &event_type, state_key, &content);

		services.db["eventid_outlierpdu"].raw_put(event_id, Json(event));

		let key = services
			.short
			.get_or_create_shortstatekey(&event_type, state_key)
			.await;

		compressed.push(
			services
				.state_compressor
				.compress_state_event(key, event_id)
				.await,
		);
	}

	let state = Arc::new(compressed.into_iter().collect());
	let hash = services
		.state
		.set_event_state(name_id, room, state)
		.await?;

	let lock = services.state.mutex.lock(room).await;

	services
		.state
		.install_force_state(room, hash, &lock);

	Ok(())
}

#[tokio::test]
async fn child_access_respects_join_rule_and_membership() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!child:localhost");
	let allowed_room = room_id!("!allowed:localhost");
	let user = user_id!("@member:localhost");
	let sender = Identifier::UserId(user);
	let empty_restriction = RestrictedSummary { allowed_room_ids: Vec::new() };
	let allowed_restriction = RestrictedSummary {
		allowed_room_ids: vec![allowed_room.to_owned()],
	};

	assert!(
		services
			.spaces
			.is_accessible_child(room, &JoinRuleSummary::Public, &sender)
			.await
	);

	assert!(
		services
			.spaces
			.is_accessible_child(room, &JoinRuleSummary::Knock, &sender)
			.await
	);

	assert!(
		services
			.spaces
			.is_accessible_child(
				room,
				&JoinRuleSummary::KnockRestricted(allowed_restriction.clone()),
				&sender,
			)
			.await
	);

	assert!(
		!services
			.spaces
			.is_accessible_child(room, &JoinRuleSummary::Invite, &sender)
			.await
	);

	assert!(
		services
			.spaces
			.is_accessible_child(room, &JoinRuleSummary::Restricted(empty_restriction), &sender,)
			.await
	);

	assert!(
		!services
			.spaces
			.is_accessible_child(
				room,
				&JoinRuleSummary::Restricted(allowed_restriction.clone()),
				&sender,
			)
			.await
	);

	transition(services, room, user, MembershipState::Invite).await?;
	assert!(
		services
			.spaces
			.is_accessible_child(room, &JoinRuleSummary::Invite, &sender)
			.await
	);

	transition(services, room, user, MembershipState::Leave).await?;
	transition(services, allowed_room, user, MembershipState::Join).await?;
	assert!(
		services
			.spaces
			.is_accessible_child(room, &JoinRuleSummary::Restricted(allowed_restriction), &sender,)
			.await
	);

	Ok(())
}

#[tokio::test]
async fn child_access_applies_server_acl_to_fresh_and_cached_summary() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!acl:localhost");
	let user = user_id!("@member:localhost");
	let server = server_name!("blocked.example");
	let sender = Identifier::ServerName(server);
	let summary = parent_summary(room.as_str());

	seed_local_summary(services, room, user).await?;
	transition(services, room, user, MembershipState::Join).await?;

	assert!(
		!services
			.spaces
			.is_accessible_child(room, &JoinRuleSummary::Public, &sender)
			.await
	);

	services
		.spaces
		.cache_put(room, Some(&summary), Provenance::Local);

	let cached = services
		.spaces
		.get_summary_and_children_local(room, &sender)
		.await?;

	assert!(matches!(cached, Accessibility::Inaccessible));

	Ok(())
}

fn seed_legacy_cache(
	services: &Services,
	room_id: &RoomId,
	summary: &SpaceHierarchyParentSummary,
) {
	let cached = Cached {
		expires: UNIX_EPOCH
			.checked_add(now())
			.and_then(|expires| expires.checked_add(Duration::from_mins(1)))
			.expect("test cache expiry is representable"),
		summary: Some(summary.clone()),
		provenance: Provenance::Unknown,
	};

	let mut legacy = serde_json::to_value(cached).unwrap();

	legacy
		.as_object_mut()
		.unwrap()
		.remove("provenance");

	services.db["roomid_spacehierarchy"].raw_put(room_id, Json(legacy));
}

#[test]
fn cache_provenance_is_backward_compatible() {
	let legacy = r#"{
		"expires":{"secs_since_epoch":0,"nanos_since_epoch":0},
		"summary":null
	}"#;

	let decoded: Cached = serde_json::from_str(legacy).unwrap();

	assert_eq!(decoded.provenance, Provenance::Unknown);

	let cached = Cached {
		expires: UNIX_EPOCH,
		summary: Some(parent_summary("!root:example.org")),
		provenance: Provenance::Remote,
	};

	let round_trip: Cached =
		serde_json::from_value(serde_json::to_value(&cached).unwrap()).unwrap();

	assert_eq!(round_trip.provenance, Provenance::Remote);
}

#[test]
fn summary_identity_is_bound_to_requested_room() {
	let summary = parent_summary("!other:example.org");

	assert!(!summary_matches_room(&summary, room_id!("!requested:example.org")));
	assert!(summary_matches_room(&summary, room_id!("!other:example.org")));
}

fn parent_summary(room_id: &str) -> SpaceHierarchyParentSummary {
	let summary = RoomSummary::new(
		room_id.try_into().unwrap(),
		JoinRuleSummary::Public,
		true,
		UInt::from(1_u32),
		true,
	);

	SpaceHierarchyParentSummary { summary, children_state: Vec::new() }
}

#[test]
fn get_summary_children() {
	let summary: SpaceHierarchyParentSummary = SpaceHierarchyParentSummary {
		summary: RoomSummary::new(
			owned_room_id!("!root:example.org"),
			JoinRuleSummary::Public,
			true,
			UInt::from(1_u32),
			true,
		),
		children_state: vec![
			serde_json::from_str(
				r#"{
                      "content": {
                        "via": [
                          "example.org"
                        ],
                        "suggested": false
                      },
                      "origin_server_ts": 1629413349153,
                      "sender": "@alice:example.org",
                      "state_key": "!foo:example.org",
                      "type": "m.space.child"
                    }"#,
			)
			.unwrap(),
			serde_json::from_str(
				r#"{
                      "content": {
                        "via": [
                          "example.org"
                        ],
                        "suggested": true
                      },
                      "origin_server_ts": 1629413349157,
                      "sender": "@alice:example.org",
                      "state_key": "!bar:example.org",
                      "type": "m.space.child"
                    }"#,
			)
			.unwrap(),
			serde_json::from_str(
				r#"{
                      "content": {
                        "via": [
                          "example.org"
                        ]
                      },
                      "origin_server_ts": 1629413349160,
                      "sender": "@alice:example.org",
                      "state_key": "!baz:example.org",
                      "type": "m.space.child"
                    }"#,
			)
			.unwrap(),
		],
	};

	assert_eq!(
		get_parent_children_via(&summary, false)
			.map(|(k, v)| (k, v.collect::<Vec<_>>()))
			.collect::<Vec<_>>(),
		vec![
			(owned_room_id!("!foo:example.org"), vec![owned_server_name!("example.org")]),
			(owned_room_id!("!bar:example.org"), vec![owned_server_name!("example.org")]),
			(owned_room_id!("!baz:example.org"), vec![owned_server_name!("example.org")])
		]
	);
	assert_eq!(
		get_parent_children_via(&summary, true)
			.map(|(k, v)| (k, v.collect::<Vec<_>>()))
			.collect::<Vec<_>>(),
		vec![(owned_room_id!("!bar:example.org"), vec![owned_server_name!("example.org")])]
	);
}

#[test]
fn invalid_pagination_tokens() {
	fn token_is_err(token: &str) { PaginationToken::from_str(token).unwrap_err(); }

	token_is_err("231_2_noabool");
	token_is_err("");
	token_is_err("111_3_");
	token_is_err("foo_not_int");
	token_is_err("11_4_true_");
	token_is_err("___");
	token_is_err("__false");
}

#[test]
fn valid_pagination_tokens() {
	assert_eq!(
		PaginationToken {
			short_room_ids: vec![5383, 42934, 283, 423],
			limit: UInt::from(20_u32),
			max_depth: UInt::from(1_u32),
			suggested_only: true
		},
		PaginationToken::from_str("5383,42934,283,423_20_1_true").unwrap()
	);

	assert_eq!(
		PaginationToken {
			short_room_ids: vec![740],
			limit: UInt::from(97_u32),
			max_depth: UInt::from(10539_u32),
			suggested_only: false
		},
		PaginationToken::from_str("740_97_10539_false").unwrap()
	);
}

#[test]
fn pagination_token_to_string() {
	assert_eq!(
		PaginationToken {
			short_room_ids: vec![740],
			limit: UInt::from(97_u32),
			max_depth: UInt::from(10539_u32),
			suggested_only: false
		}
		.to_string(),
		"740_97_10539_false"
	);

	assert_eq!(
		PaginationToken {
			short_room_ids: vec![9, 34],
			limit: UInt::from(3_u32),
			max_depth: UInt::from(1_u32),
			suggested_only: true
		}
		.to_string(),
		"9,34_3_1_true"
	);
}
