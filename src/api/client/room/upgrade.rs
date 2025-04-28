use std::cmp::max;

use axum::extract::State;
use futures::StreamExt;
use ruma::{
	CanonicalJsonObject, RoomId, RoomVersionId,
	api::client::{error::ErrorKind, room::upgrade_room},
	events::{
		StateEventType, TimelineEventType,
		room::{
			member::{MembershipState, RoomMemberEventContent},
			power_levels::RoomPowerLevelsEventContent,
			tombstone::RoomTombstoneEventContent,
		},
	},
	int,
};
use serde_json::{json, value::to_raw_value};
use tuwunel_core::{
	Error, Result, err, info,
	matrix::{StateKey, pdu::PduBuilder},
};

use crate::Ruma;

/// Recommended transferable state events list from the spec
const TRANSFERABLE_STATE_EVENTS: &[StateEventType; 9] = &[
	StateEventType::RoomAvatar,
	StateEventType::RoomEncryption,
	StateEventType::RoomGuestAccess,
	StateEventType::RoomHistoryVisibility,
	StateEventType::RoomJoinRules,
	StateEventType::RoomName,
	StateEventType::RoomPowerLevels,
	StateEventType::RoomServerAcl,
	StateEventType::RoomTopic,
];

/// # `POST /_matrix/client/r0/rooms/{roomId}/upgrade`
///
/// Upgrades the room.
///
/// - Creates a replacement room
/// - Sends a tombstone event into the current room
/// - Sender user joins the room
/// - Transfers some state events
/// - Moves local aliases
/// - Modifies old room power levels to prevent users from speaking
pub(crate) async fn upgrade_room_route(
	State(services): State<crate::State>,
	body: Ruma<upgrade_room::v3::Request>,
) -> Result<upgrade_room::v3::Response> {
	debug_assert!(
		TRANSFERABLE_STATE_EVENTS.is_sorted(),
		"TRANSFERABLE_STATE_EVENTS is not sorted"
	);
	let sender_user = body
		.sender_user
		.as_ref()
		.expect("user is authenticated");

	if !services
		.server
		.supported_room_version(&body.new_version)
	{
		return Err(Error::BadRequest(
			ErrorKind::UnsupportedRoomVersion,
			"This server does not support that room version.",
		));
	}

	// Create a replacement room
	let replacement_room = RoomId::new(services.globals.server_name());

	let _short_id = services
		.rooms
		.short
		.get_or_create_shortroomid(&replacement_room)
		.await;

	let state_lock = services
		.rooms
		.state
		.mutex
		.lock(&body.room_id)
		.await;

	// Send a m.room.tombstone event to the old room to indicate that it is not
	// intended to be used any further Fail if the sender does not have the required
	// permissions
	let tombstone_event_id = services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				StateKey::new(),
				&RoomTombstoneEventContent {
					body: "This room has been replaced".to_owned(),
					replacement_room: replacement_room.clone(),
				},
			),
			sender_user,
			&body.room_id,
			&state_lock,
		)
		.await?;

	// Change lock to replacement room
	drop(state_lock);
	let state_lock = services
		.rooms
		.state
		.mutex
		.lock(&replacement_room)
		.await;

	// Get the old room creation event
	let mut create_event_content: CanonicalJsonObject = services
		.rooms
		.state_accessor
		.room_state_get_content(&body.room_id, &StateEventType::RoomCreate, "")
		.await
		.map_err(|_| err!(Database("Found room without m.room.create event.")))?;

	// Use the m.room.tombstone event as the predecessor
	let predecessor = Some(ruma::events::room::create::PreviousRoom::new(
		body.room_id.clone(),
		Some(tombstone_event_id),
	));

	// Send a m.room.create event containing a predecessor field and the applicable
	// room_version
	{
		use RoomVersionId::*;
		match body.new_version {
			| V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 => {
				create_event_content.insert(
					"creator".into(),
					json!(&sender_user).try_into().map_err(|e| {
						info!("Error forming creation event: {e}");
						Error::BadRequest(ErrorKind::BadJson, "Error forming creation event")
					})?,
				);
			},
			| _ => {
				// "creator" key no longer exists in V11+ rooms
				create_event_content.remove("creator");
			},
		}
	}

	create_event_content.insert(
		"room_version".into(),
		json!(&body.new_version)
			.try_into()
			.map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Error forming creation event"))?,
	);
	create_event_content.insert(
		"predecessor".into(),
		json!(predecessor)
			.try_into()
			.map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Error forming creation event"))?,
	);

	// Validate creation event content
	if serde_json::from_str::<CanonicalJsonObject>(
		to_raw_value(&create_event_content)
			.expect("Error forming creation event")
			.get(),
	)
	.is_err()
	{
		return Err(Error::BadRequest(ErrorKind::BadJson, "Error forming creation event"));
	}

	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomCreate,
				content: to_raw_value(&create_event_content)
					.expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(StateKey::new()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&replacement_room,
			&state_lock,
		)
		.await?;

	// Join the new room
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomMember,
				content: to_raw_value(&RoomMemberEventContent {
					membership: MembershipState::Join,
					displayname: services.users.displayname(sender_user).await.ok(),
					avatar_url: services.users.avatar_url(sender_user).await.ok(),
					is_direct: None,
					third_party_invite: None,
					blurhash: services.users.blurhash(sender_user).await.ok(),
					reason: None,
					join_authorized_via_users_server: None,
				})
				.expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(sender_user.as_str().into()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&replacement_room,
			&state_lock,
		)
		.await?;

	// Replicate transferable state events to the new room
	for event_type in TRANSFERABLE_STATE_EVENTS {
		let event_content = match services
			.rooms
			.state_accessor
			.room_state_get(&body.room_id, event_type, "")
			.await
		{
			| Ok(v) => v.content.clone(),
			| Err(_) => continue, // Skipping missing events.
		};

		services
			.rooms
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					event_type: event_type.to_string().into(),
					content: event_content,
					state_key: Some(StateKey::new()),
					..Default::default()
				},
				sender_user,
				&replacement_room,
				&state_lock,
			)
			.await?;
	}

	// Moves any local aliases to the new room
	let mut local_aliases = services
		.rooms
		.alias
		.local_aliases_for_room(&body.room_id)
		.boxed();

	while let Some(alias) = local_aliases.next().await {
		services
			.rooms
			.alias
			.remove_alias(alias, sender_user)
			.await?;

		services
			.rooms
			.alias
			.set_alias(alias, &replacement_room, sender_user)?;
	}

	// Get the old room power levels
	let power_levels_event_content: RoomPowerLevelsEventContent = services
		.rooms
		.state_accessor
		.room_state_get_content(&body.room_id, &StateEventType::RoomPowerLevels, "")
		.await
		.map_err(|_| err!(Database("Found room without m.room.power_levels event.")))?;

	// Setting events_default and invite to the greater of 50 and users_default + 1
	let new_level = max(
		int!(50),
		power_levels_event_content
			.users_default
			.checked_add(int!(1))
			.ok_or_else(|| {
				err!(Request(BadJson("users_default power levels event content is not valid")))
			})?,
	);

	// Modify the power levels in the old room to prevent sending of events and
	// inviting new users
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				StateKey::new(),
				&RoomPowerLevelsEventContent {
					events_default: new_level,
					invite: new_level,
					..power_levels_event_content
				},
			),
			sender_user,
			&body.room_id,
			&state_lock,
		)
		.await?;

	drop(state_lock);

	// Return the replacement room id
	Ok(upgrade_room::v3::Response { replacement_room })
}
