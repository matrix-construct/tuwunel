use std::collections::BTreeMap;

use axum::extract::State;
use futures::FutureExt;
use ruma::{
	CanonicalJsonObject, Int, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId,
	api::client::{
		error::ErrorKind,
		room::{self, create_room},
	},
	events::{
		TimelineEventType,
		room::{
			canonical_alias::RoomCanonicalAliasEventContent,
			create::RoomCreateEventContent,
			guest_access::{GuestAccess, RoomGuestAccessEventContent},
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
			name::RoomNameEventContent,
			power_levels::RoomPowerLevelsEventContent,
			topic::RoomTopicEventContent,
		},
	},
	int,
	serde::{JsonObject, Raw},
};
use serde_json::{json, value::to_raw_value};
use tuwunel_core::{
	Err, Error, Result, debug_info, debug_warn, err, error, info,
	matrix::{StateKey, pdu::PduBuilder},
	warn,
};
use tuwunel_service::{Services, appservice::RegistrationInfo};

use crate::{Ruma, client::invite_helper};

/// # `POST /_matrix/client/v3/createRoom`
///
/// Creates a new room.
///
/// - Room ID is randomly generated
/// - Create alias if `room_alias_name` is set
/// - Send create event
/// - Join sender user
/// - Send power levels event
/// - Send canonical room alias
/// - Send join rules
/// - Send history visibility
/// - Send guest access
/// - Send events listed in initial state
/// - Send events implied by `name` and `topic`
/// - Send invite events
#[allow(clippy::large_stack_frames)]
pub(crate) async fn create_room_route(
	State(services): State<crate::State>,
	body: Ruma<create_room::v3::Request>,
) -> Result<create_room::v3::Response> {
	use create_room::v3::RoomPreset;

	let sender_user = body
		.sender_user
		.as_ref()
		.expect("user is authenticated");

	if !services.globals.allow_room_creation()
		&& body.appservice_info.is_none()
		&& !services.users.is_admin(sender_user).await
	{
		return Err(Error::BadRequest(
			ErrorKind::forbidden(),
			"Room creation has been disabled.",
		));
	}

	let room_id: OwnedRoomId = match &body.room_id {
		| Some(custom_room_id) => custom_room_id_check(&services, custom_room_id)?,
		| _ => RoomId::new(&services.server.name),
	};

	// check if room ID doesn't already exist instead of erroring on auth check
	if services
		.rooms
		.short
		.get_shortroomid(&room_id)
		.await
		.is_ok()
	{
		return Err(Error::BadRequest(
			ErrorKind::RoomInUse,
			"Room with that custom room ID already exists",
		));
	}

	if body.visibility == room::Visibility::Public
		&& services
			.server
			.config
			.lockdown_public_room_directory
		&& !services.users.is_admin(sender_user).await
		&& body.appservice_info.is_none()
	{
		info!(
			"Non-admin user {sender_user} tried to publish {0} to the room directory while \
			 \"lockdown_public_room_directory\" is enabled",
			&room_id
		);

		if services.server.config.admin_room_notices {
			services
				.admin
				.send_text(&format!(
					"Non-admin user {sender_user} tried to publish {0} to the room directory \
					 while \"lockdown_public_room_directory\" is enabled",
					&room_id
				))
				.await;
		}

		return Err!(Request(Forbidden("Publishing rooms to the room directory is not allowed")));
	}

	let _short_id = services
		.rooms
		.short
		.get_or_create_shortroomid(&room_id)
		.await;
	let state_lock = services.rooms.state.mutex.lock(&room_id).await;

	let alias: Option<OwnedRoomAliasId> = match body.room_alias_name.as_ref() {
		| Some(alias) => {
			Some(room_alias_check(&services, alias, body.appservice_info.as_ref()).await?)
		},
		| _ => None,
	};

	let room_version = match body.room_version.clone() {
		| Some(room_version) => {
			if services
				.server
				.supported_room_version(&room_version)
			{
				room_version
			} else {
				return Err(Error::BadRequest(
					ErrorKind::UnsupportedRoomVersion,
					"This server does not support that room version.",
				));
			}
		},
		| None => services
			.server
			.config
			.default_room_version
			.clone(),
	};

	let create_content = match &body.creation_content {
		| Some(content) => {
			use RoomVersionId::*;

			let mut content = content
				.deserialize_as::<CanonicalJsonObject>()
				.map_err(|e| {
					error!("Failed to deserialise content as canonical JSON: {}", e);
					Error::bad_database("Failed to deserialise content as canonical JSON.")
				})?;
			match room_version {
				| V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 => {
					content.insert(
						"creator".into(),
						json!(&sender_user).try_into().map_err(|e| {
							info!("Invalid creation content: {e}");
							Error::BadRequest(ErrorKind::BadJson, "Invalid creation content")
						})?,
					);
				},
				| _ => {
					// V11+ removed the "creator" key
				},
			}
			content.insert(
				"room_version".into(),
				json!(room_version.as_str())
					.try_into()
					.map_err(|_| {
						Error::BadRequest(ErrorKind::BadJson, "Invalid creation content")
					})?,
			);
			content
		},
		| None => {
			use RoomVersionId::*;

			let content = match room_version {
				| V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 => {
					RoomCreateEventContent::new_v1(sender_user.clone())
				},
				| _ => RoomCreateEventContent::new_v11(),
			};
			let mut content = serde_json::from_str::<CanonicalJsonObject>(
				to_raw_value(&content)
					.expect("we just created this as content was None")
					.get(),
			)
			.unwrap();
			content.insert(
				"room_version".into(),
				json!(room_version.as_str())
					.try_into()
					.expect("we just created this as content was None"),
			);
			content
		},
	};

	// 1. The room create event
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomCreate,
				content: to_raw_value(&create_content)
					.expect("create event content serialization"),
				state_key: Some(StateKey::new()),
				..Default::default()
			},
			sender_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 2. Let the room creator join
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				sender_user.to_string(),
				&RoomMemberEventContent {
					displayname: services.users.displayname(sender_user).await.ok(),
					avatar_url: services.users.avatar_url(sender_user).await.ok(),
					blurhash: services.users.blurhash(sender_user).await.ok(),
					is_direct: Some(body.is_direct),
					..RoomMemberEventContent::new(MembershipState::Join)
				},
			),
			sender_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 3. Power levels

	// Figure out preset. We need it for preset specific events
	let preset = body
		.preset
		.clone()
		.unwrap_or(match &body.visibility {
			| room::Visibility::Public => RoomPreset::PublicChat,
			| _ => RoomPreset::PrivateChat, // Room visibility should not be custom
		});

	let mut users = BTreeMap::from_iter([(sender_user.clone(), int!(100))]);

	if preset == RoomPreset::TrustedPrivateChat {
		for invite in &body.invite {
			if services
				.users
				.user_is_ignored(sender_user, invite)
				.await
			{
				continue;
			} else if services
				.users
				.user_is_ignored(invite, sender_user)
				.await
			{
				// silently drop the invite to the recipient if they've been ignored by the
				// sender, pretend it worked
				continue;
			}

			users.insert(invite.clone(), int!(100));
		}
	}

	let power_levels_content = default_power_levels_content(
		body.power_level_content_override.as_ref(),
		&body.visibility,
		users,
	)?;

	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomPowerLevels,
				content: to_raw_value(&power_levels_content)
					.expect("serialized power_levels event content"),
				state_key: Some(StateKey::new()),
				..Default::default()
			},
			sender_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 4. Canonical room alias
	if let Some(room_alias_id) = &alias {
		services
			.rooms
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(
					String::new(),
					&RoomCanonicalAliasEventContent {
						alias: Some(room_alias_id.to_owned()),
						alt_aliases: vec![],
					},
				),
				sender_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;
	}

	// 5. Events set by preset

	// 5.1 Join Rules
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::new(),
				&RoomJoinRulesEventContent::new(match preset {
					| RoomPreset::PublicChat => JoinRule::Public,
					// according to spec "invite" is the default
					| _ => JoinRule::Invite,
				}),
			),
			sender_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 5.2 History Visibility
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::new(),
				&RoomHistoryVisibilityEventContent::new(HistoryVisibility::Shared),
			),
			sender_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 5.3 Guest Access
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::new(),
				&RoomGuestAccessEventContent::new(match preset {
					| RoomPreset::PublicChat => GuestAccess::Forbidden,
					| _ => GuestAccess::CanJoin,
				}),
			),
			sender_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 6. Events listed in initial_state
	for event in &body.initial_state {
		let mut pdu_builder = event
			.deserialize_as::<PduBuilder>()
			.map_err(|e| {
				warn!("Invalid initial state event: {:?}", e);
				Error::BadRequest(ErrorKind::InvalidParam, "Invalid initial state event.")
			})?;

		debug_info!("Room creation initial state event: {event:?}");

		// client/appservice workaround: if a user sends an initial_state event with a
		// state event in there with the content of literally `{}` (not null or empty
		// string), let's just skip it over and warn.
		if pdu_builder.content.get().eq("{}") {
			info!("skipping empty initial state event with content of `{{}}`: {event:?}");
			debug_warn!("content: {}", pdu_builder.content.get());
			continue;
		}

		// Implicit state key defaults to ""
		pdu_builder
			.state_key
			.get_or_insert_with(StateKey::new);

		// Silently skip encryption events if they are not allowed
		if pdu_builder.event_type == TimelineEventType::RoomEncryption
			&& !services.config.allow_encryption
		{
			continue;
		}

		services
			.rooms
			.timeline
			.build_and_append_pdu(pdu_builder, sender_user, &room_id, &state_lock)
			.boxed()
			.await?;
	}

	// 7. Events implied by name and topic
	if let Some(name) = &body.name {
		services
			.rooms
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(String::new(), &RoomNameEventContent::new(name.clone())),
				sender_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;
	}

	if let Some(topic) = &body.topic {
		services
			.rooms
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(String::new(), &RoomTopicEventContent { topic: topic.clone() }),
				sender_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;
	}

	// 8. Events implied by invite (and TODO: invite_3pid)
	drop(state_lock);
	for user_id in &body.invite {
		if services
			.users
			.user_is_ignored(sender_user, user_id)
			.await
		{
			continue;
		} else if services
			.users
			.user_is_ignored(user_id, sender_user)
			.await
		{
			// silently drop the invite to the recipient if they've been ignored by the
			// sender, pretend it worked
			continue;
		}

		if let Err(e) =
			invite_helper(&services, sender_user, user_id, &room_id, None, body.is_direct)
				.boxed()
				.await
		{
			warn!(%e, "Failed to send invite");
		}
	}

	// Homeserver specific stuff
	if let Some(alias) = alias {
		services
			.rooms
			.alias
			.set_alias(&alias, &room_id, sender_user)?;
	}

	if body.visibility == room::Visibility::Public {
		services.rooms.directory.set_public(&room_id);

		if services.server.config.admin_room_notices {
			services
				.admin
				.send_text(&format!(
					"{sender_user} made {} public to the room directory",
					&room_id
				))
				.await;
		}
		info!("{sender_user} made {0} public to the room directory", &room_id);
	}

	info!("{sender_user} created a room with room ID {room_id}");

	Ok(create_room::v3::Response::new(room_id))
}

/// creates the power_levels_content for the PDU builder
fn default_power_levels_content(
	power_level_content_override: Option<&Raw<RoomPowerLevelsEventContent>>,
	visibility: &room::Visibility,
	users: BTreeMap<OwnedUserId, Int>,
) -> Result<serde_json::Value> {
	let mut power_levels_content =
		serde_json::to_value(RoomPowerLevelsEventContent { users, ..Default::default() })
			.expect("event is valid, we just created it");

	// secure proper defaults of sensitive/dangerous permissions that moderators
	// (power level 50) should not have easy access to
	power_levels_content["events"]["m.room.power_levels"] =
		serde_json::to_value(100).expect("100 is valid Value");
	power_levels_content["events"]["m.room.server_acl"] =
		serde_json::to_value(100).expect("100 is valid Value");
	power_levels_content["events"]["m.room.tombstone"] =
		serde_json::to_value(100).expect("100 is valid Value");
	power_levels_content["events"]["m.room.encryption"] =
		serde_json::to_value(100).expect("100 is valid Value");
	power_levels_content["events"]["m.room.history_visibility"] =
		serde_json::to_value(100).expect("100 is valid Value");

	// always allow users to respond (not post new) to polls. this is primarily
	// useful in read-only announcement rooms that post a public poll.
	power_levels_content["events"]["org.matrix.msc3381.poll.response"] =
		serde_json::to_value(0).expect("0 is valid Value");
	power_levels_content["events"]["m.poll.response"] =
		serde_json::to_value(0).expect("0 is valid Value");

	// synapse does this too. clients do not expose these permissions. it prevents
	// default users from calling public rooms, for obvious reasons.
	if *visibility == room::Visibility::Public {
		power_levels_content["events"]["m.call.invite"] =
			serde_json::to_value(50).expect("50 is valid Value");
		power_levels_content["events"]["m.call"] =
			serde_json::to_value(50).expect("50 is valid Value");
		power_levels_content["events"]["m.call.member"] =
			serde_json::to_value(50).expect("50 is valid Value");
		power_levels_content["events"]["org.matrix.msc3401.call"] =
			serde_json::to_value(50).expect("50 is valid Value");
		power_levels_content["events"]["org.matrix.msc3401.call.member"] =
			serde_json::to_value(50).expect("50 is valid Value");
	}

	if let Some(power_level_content_override) = power_level_content_override {
		let json: JsonObject = serde_json::from_str(power_level_content_override.json().get())
			.map_err(|_| {
				Error::BadRequest(ErrorKind::BadJson, "Invalid power_level_content_override.")
			})?;

		for (key, value) in json {
			power_levels_content[key] = value;
		}
	}

	Ok(power_levels_content)
}

/// if a room is being created with a room alias, run our checks
async fn room_alias_check(
	services: &Services,
	room_alias_name: &str,
	appservice_info: Option<&RegistrationInfo>,
) -> Result<OwnedRoomAliasId> {
	// Basic checks on the room alias validity
	if room_alias_name.contains(':') {
		return Err(Error::BadRequest(
			ErrorKind::InvalidParam,
			"Room alias contained `:` which is not allowed. Please note that this expects a \
			 localpart, not the full room alias.",
		));
	} else if room_alias_name.contains(char::is_whitespace) {
		return Err(Error::BadRequest(
			ErrorKind::InvalidParam,
			"Room alias contained spaces which is not a valid room alias.",
		));
	}

	// check if room alias is forbidden
	if services
		.globals
		.forbidden_alias_names()
		.is_match(room_alias_name)
	{
		return Err(Error::BadRequest(ErrorKind::Unknown, "Room alias name is forbidden."));
	}

	let server_name = services.globals.server_name();
	let full_room_alias = OwnedRoomAliasId::parse(format!("#{room_alias_name}:{server_name}"))
		.map_err(|e| {
			err!(Request(InvalidParam(debug_error!(
				?e,
				?room_alias_name,
				"Failed to parse room alias.",
			))))
		})?;

	if services
		.rooms
		.alias
		.resolve_local_alias(&full_room_alias)
		.await
		.is_ok()
	{
		return Err(Error::BadRequest(ErrorKind::RoomInUse, "Room alias already exists."));
	}

	if let Some(info) = appservice_info {
		if !info.aliases.is_match(full_room_alias.as_str()) {
			return Err(Error::BadRequest(
				ErrorKind::Exclusive,
				"Room alias is not in namespace.",
			));
		}
	} else if services
		.appservice
		.is_exclusive_alias(&full_room_alias)
		.await
	{
		return Err(Error::BadRequest(
			ErrorKind::Exclusive,
			"Room alias reserved by appservice.",
		));
	}

	debug_info!("Full room alias: {full_room_alias}");

	Ok(full_room_alias)
}

/// if a room is being created with a custom room ID, run our checks against it
fn custom_room_id_check(services: &Services, custom_room_id: &str) -> Result<OwnedRoomId> {
	// apply forbidden room alias checks to custom room IDs too
	if services
		.globals
		.forbidden_alias_names()
		.is_match(custom_room_id)
	{
		return Err(Error::BadRequest(ErrorKind::Unknown, "Custom room ID is forbidden."));
	}

	if custom_room_id.contains(':') {
		return Err(Error::BadRequest(
			ErrorKind::InvalidParam,
			"Custom room ID contained `:` which is not allowed. Please note that this expects a \
			 localpart, not the full room ID.",
		));
	} else if custom_room_id.contains(char::is_whitespace) {
		return Err(Error::BadRequest(
			ErrorKind::InvalidParam,
			"Custom room ID contained spaces which is not valid.",
		));
	}

	let server_name = services.globals.server_name();
	let full_room_id = format!("!{custom_room_id}:{server_name}");

	OwnedRoomId::parse(full_room_id)
		.map_err(Into::into)
		.inspect(|full_room_id| debug_info!(?full_room_id, "Full custom room ID"))
		.inspect_err(|e| warn!(?e, ?custom_room_id, "Failed to create room with custom room ID",))
}
