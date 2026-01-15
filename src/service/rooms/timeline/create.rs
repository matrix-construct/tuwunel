use std::cmp;

use futures::{StreamExt, TryStreamExt};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, MilliSecondsSinceUnixEpoch, OwnedEventId,
	OwnedRoomId, RoomId, UserId,
	events::{StateEventType, TimelineEventType, room::create::RoomCreateEventContent},
	room_version_rules::RoomIdFormatVersion,
	uint,
};
use serde_json::value::to_raw_value;
use tuwunel_core::{
	Error, Result, err, implement,
	matrix::{
		event::{Event, StateKey, TypeExt},
		pdu::{EventHash, PduBuilder, PduEvent, PrevEvents},
		room_version,
		state_res::{self},
	},
	utils::{
		IterStream, ReadyExt, TryReadyExt, millis_since_unix_epoch, stream::TryIgnore,
		to_canonical_object,
	},
};

use super::RoomMutexGuard;

#[implement(super::Service)]
pub async fn create_hash_and_sign_event(
	&self,
	pdu_builder: PduBuilder,
	sender: &UserId,
	room_id: &RoomId,
	// Take mutex guard to make sure users get the room state mutex
	_mutex_lock: &RoomMutexGuard,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let PduBuilder {
		event_type,
		content,
		unsigned,
		state_key,
		redacts,
		timestamp,
	} = pdu_builder;

	let prev_events: PrevEvents = self
		.services
		.state
		.get_forward_extremities(room_id)
		.take(20)
		.map(Into::into)
		.collect()
		.await;

	// If there was no create event yet, assume we are creating a room
	let (room_version, version_rules) = self
		.services
		.state
		.get_room_version(room_id)
		.await
		.or_else(|_| {
			if event_type == TimelineEventType::RoomCreate {
				let content: RoomCreateEventContent = serde_json::from_str(content.get())?;
				Ok(content.room_version)
			} else {
				Err(Error::InconsistentRoomState(
					"non-create event for room of unknown version",
					room_id.to_owned(),
				))
			}
		})
		.and_then(|room_version| {
			Ok((room_version.clone(), room_version::rules(&room_version)?))
		})?;

	let auth_events = self
		.services
		.state
		.get_auth_events(
			room_id,
			&event_type,
			sender,
			state_key.as_deref(),
			&content,
			&version_rules.authorization,
			true,
		)
		.await?;

	// Our depth is the maximum depth of prev_events + 1
	let depth = prev_events
		.iter()
		.stream()
		.map(Ok)
		.and_then(|event_id| self.get_pdu(event_id))
		.ready_and_then(|pdu| Ok(pdu.depth))
		.ignore_err()
		.ready_fold(uint!(0), cmp::max)
		.await
		.saturating_add(uint!(1));

	let mut unsigned = unsigned.unwrap_or_default();
	if let Some(state_key) = &state_key
		&& let Ok(prev_pdu) = self
			.services
			.state_accessor
			.room_state_get(room_id, &event_type.to_string().into(), state_key)
			.await
	{
		unsigned.insert("prev_content".to_owned(), prev_pdu.get_content_as_value());
		unsigned.insert("prev_sender".to_owned(), serde_json::to_value(prev_pdu.sender())?);
		unsigned.insert("replaces_state".to_owned(), serde_json::to_value(prev_pdu.event_id())?);
	}

	let unsigned = unsigned
		.is_empty()
		.eq(&false)
		.then_some(to_raw_value(&unsigned)?);

	let origin_server_ts = timestamp
		.as_ref()
		.map(MilliSecondsSinceUnixEpoch::get)
		.unwrap_or_else(|| {
			millis_since_unix_epoch()
				.try_into()
				.expect("u64 to UInt")
		});

	let mut pdu = PduEvent {
		event_id: ruma::event_id!("$thiswillbereplaced").into(),
		room_id: room_id.to_owned(),
		sender: sender.to_owned(),
		origin: Some(self.services.globals.server_name().to_owned()),
		origin_server_ts,
		kind: event_type,
		content,
		state_key,
		depth,
		redacts,
		unsigned,
		hashes: EventHash::default(),
		signatures: None,
		prev_events,
		auth_events: auth_events
			.values()
			.filter(|pdu| {
				version_rules
					.event_format
					.allow_room_create_in_auth_events
					|| *pdu.kind() != TimelineEventType::RoomCreate
			})
			.map(|pdu| pdu.event_id.clone())
			.collect(),
	};

	let auth_fetch = async |k: StateEventType, s: StateKey| {
		auth_events
			.get(&k.with_state_key(s.as_str()))
			.map(ToOwned::to_owned)
			.ok_or_else(|| err!(Request(NotFound("Missing auth events"))))
	};

	state_res::auth_check(
		&version_rules,
		&pdu,
		&async |event_id: OwnedEventId| self.get_pdu(&event_id).await,
		&auth_fetch,
	)
	.await?;

	// Hash and sign
	let mut pdu_json = to_canonical_object(&pdu).map_err(|e| {
		err!(Request(BadJson(warn!("Failed to convert PDU to canonical JSON: {e}"))))
	})?;

	// room v12 and above removed the placeholder "room_id" field from m.room.create
	if !version_rules
		.event_format
		.require_room_create_room_id
		&& pdu.kind == TimelineEventType::RoomCreate
	{
		pdu_json.remove("room_id");
	}

	pdu.event_id = self
		.services
		.server_keys
		.gen_id_hash_and_sign_event(&mut pdu_json, &room_version)?;

	// Room id is event id for V12+
	if matches!(version_rules.room_id_format, RoomIdFormatVersion::V2)
		&& pdu.kind == TimelineEventType::RoomCreate
	{
		pdu.room_id = OwnedRoomId::from_parts('!', pdu.event_id.localpart(), None)?;
		pdu_json.insert("room_id".into(), CanonicalJsonValue::String(pdu.room_id.clone().into()));
	}

	state_res::check_pdu_format(&pdu_json, &version_rules.event_format)?;

	// Generate short event id
	let _shorteventid = self
		.services
		.short
		.get_or_create_shorteventid(&pdu.event_id)
		.await;

	Ok((pdu, pdu_json))
}
