use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, RoomId, RoomVersionId,
	api::error::ErrorKind, events::room::member::RoomMemberEventContent, room_id,
};
use serde_json::{Value, from_value, json, to_vec, value::to_raw_value};
use tuwunel_core::{Error, Result, config::Figment};
use tuwunel_matrix::room_version::rules as room_version_rules;

use super::merge_member_content;
use crate::{
	Services,
	test_utils::{keyed_fixture, pdu_id, signed_state, with_field},
};

const MAX_BYTES: usize = 4096;

/// A send_join event is stored without its `unsigned` data, and redacted when
/// its content no longer matches its hash.
///
/// An event of another room in the same response is not stored.
#[tokio::test]
async fn an_altered_send_join_event_is_stored_redacted() -> Result {
	let Some(fixture) = keyed_fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!join:localhost");
	let topic = json!({"topic": "signed"});
	let name = json!({"name": "signed"});
	let (topic_id, topic_event) = signed_state(services, room_id, "m.room.topic", "", &topic)?;
	let (name_id, name_event) = signed_state(services, room_id, "m.room.name", "", &name)?;
	let (foreign_id, foreign) =
		signed_state(services, room_id!("!other:localhost"), "m.room.topic", "", &topic)?;

	let altered = with_field(topic_event, "content", json!({"topic": "altered"}));
	let annotated = with_field(name_event, "unsigned", json!({"age": 1}));

	ingest(services, room_id, &[altered, annotated, foreign]).await?;

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
	let Some(fixture) = keyed_fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!join:localhost");
	let signed = |kind, content| signed_state(services, room_id, kind, "", &content);
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

	ingest(services, room_id, &[accepted, altered, genuine]).await?;

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

/// A stored profile avatar past its limit rides along a join with small extras.
///
/// The stored value predates the limit, so the join is not measured over it.
#[test]
fn stored_profile_fields_pass_beside_small_extras() -> Result {
	let avatar = oversized_avatar();
	let extra = extras(json!({ "com.example.key": "value" }));

	let merged = merge(member(Some("name"), Some(&avatar)), Some(&extra))?;

	assert_eq!(field(&merged, "avatar_url"), Some(avatar.as_str()));
	assert_eq!(field(&merged, "com.example.key"), Some("value"));

	Ok(())
}

#[test]
fn extras_past_the_cap_are_refused() {
	let extra = extras(json!({ "com.example.pad": "p".repeat(MAX_BYTES) }));
	assert!(refused(&extra));
}

#[test]
fn a_client_supplied_name_is_bounded_by_characters() -> Result {
	let long = extras(json!({ "displayname": "n".repeat(257) }));
	assert!(refused(&long));

	let fitting = extras(json!({ "displayname": "n".repeat(256) }));

	merge(member(None, None), Some(&fitting))?;

	Ok(())
}

/// A name the client supplies is bounded only while the profile has none.
///
/// Beside a stored name the client's name never reaches the merged event, so it
/// is not bounded.
#[test]
fn a_client_name_beside_a_stored_name_is_not_bounded() -> Result {
	let stored = oversized_name();
	let long = extras(json!({ "displayname": oversized_name() }));

	let merged = merge(member(Some(&stored), None), Some(&long))?;

	assert_eq!(field(&merged, "displayname"), Some(stored.as_str()));

	Ok(())
}

#[test]
fn a_stored_name_past_its_budget_passes_beside_extras() -> Result {
	let name = oversized_name();
	let extra = extras(json!({ "com.example.key": "value" }));

	let merged = merge(member(Some(&name), None), Some(&extra))?;

	assert_eq!(field(&merged, "displayname"), Some(name.as_str()));

	Ok(())
}

#[test]
fn without_extras_nothing_is_measured() -> Result {
	let name = oversized_name();
	let avatar = oversized_avatar();

	merge(member(Some(&name), Some(&avatar)), None)?;

	Ok(())
}

#[test]
fn a_client_avatar_past_its_budget_is_refused_without_a_profile_avatar() -> Result {
	let over = avatar_of_len(1001);
	let extra = extras(json!({ "avatar_url": over }));
	let error = merge(member(Some("name"), None), Some(&extra)).unwrap_err();

	assert!(bad_json(&error));

	let fitting = avatar_of_len(1000);
	let extra = extras(json!({ "avatar_url": fitting }));

	merge(member(Some("name"), None), Some(&extra))?;

	Ok(())
}

#[test]
fn a_stored_avatar_wins_over_the_clients_avatar() -> Result {
	let stored = oversized_avatar();
	let extra = extras(json!({ "avatar_url": avatar_of_len(2016) }));

	let merged = merge(member(Some("name"), Some(&stored)), Some(&extra))?;

	assert_eq!(field(&merged, "avatar_url"), Some(stored.as_str()));

	Ok(())
}

#[test]
fn reserved_join_keys_are_dropped_from_the_extras() -> Result {
	let extra = extras(json!({
		"reason": "client reason",
		"third_party_signed": { "sender": "@user:localhost" },
		"join_authorised_via_users_server": "@user:localhost",
		"com.example.key": "value",
	}));

	let merged = merge(member(Some("name"), None), Some(&extra))?;

	assert!(matches!(
		&merged,
		CanonicalJsonValue::Object(object)
			if !object.contains_key("reason")
				&& !object.contains_key("third_party_signed")
				&& !object.contains_key("join_authorised_via_users_server")
				&& object.contains_key("com.example.key")
	));

	Ok(())
}

async fn ingest(services: &Services, room_id: &RoomId, events: &[Value]) -> Result {
	let version = RoomVersionId::V11;
	let auth_chain = events
		.iter()
		.map(to_raw_value)
		.collect::<Result<Vec<_>, _>>()?;

	services
		.membership
		.ingest_send_join_auth_chain(
			room_id,
			&version,
			&room_version_rules(&version)?,
			&auth_chain,
		)
		.await;

	Ok(())
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

fn extras(value: Value) -> CanonicalJsonObject { from_value(value).expect("a canonical object") }

fn refused(extra: &CanonicalJsonObject) -> bool {
	merge(member(None, None), Some(extra)).is_err_and(|error| bad_json(&error))
}

fn merge(
	content: RoomMemberEventContent,
	extra: Option<&CanonicalJsonObject>,
) -> Result<CanonicalJsonValue> {
	merge_member_content(MAX_BYTES, content, extra)
}

fn member(displayname: Option<&str>, avatar_url: Option<&str>) -> RoomMemberEventContent {
	from_value(json!({
		"membership": "join",
		"displayname": displayname,
		"avatar_url": avatar_url,
	}))
	.expect("a join member content")
}

fn field<'a>(merged: &'a CanonicalJsonValue, key: &str) -> Option<&'a str> {
	merged
		.as_object()
		.and_then(|object| object.get(key))
		.and_then(CanonicalJsonValue::as_str)
}

fn bad_json(error: &Error) -> bool { matches!(error.kind(), ErrorKind::BadJson) }

fn oversized_name() -> String { "n".repeat(300) }

fn oversized_avatar() -> String { avatar_of_len(1516) }

fn avatar_of_len(len: usize) -> String {
	const PREFIX: &str = "mxc://localhost/";

	format!("{PREFIX}{}", "a".repeat(len.saturating_sub(PREFIX.len())))
}
