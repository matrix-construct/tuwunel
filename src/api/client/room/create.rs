use std::collections::BTreeMap;

use axum::extract::State;
use futures::FutureExt;
use ruma::{
	CanonicalJsonObject, EventEncryptionAlgorithm, Int, OwnedRoomAliasId, OwnedRoomId,
	OwnedUserId, RoomId, RoomVersionId,
	api::client::room::{
		self, create_room,
		create_room::v3::{CreationContent, RoomPreset},
	},
	events::{
		TimelineEventType,
		room::{
			canonical_alias::RoomCanonicalAliasEventContent,
			create::RoomCreateEventContent,
			encryption::RoomEncryptionEventContent,
			guest_access::{GuestAccess, RoomGuestAccessEventContent},
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
			name::RoomNameEventContent,
			power_levels::RoomPowerLevelsEventContent,
			topic::{RoomTopicEventContent, TopicContentBlock},
		},
	},
	int,
	room_version_rules::{RoomIdFormatVersion, RoomVersionRules},
	serde::{JsonObject, Raw},
};
use serde_json::{json, value::to_raw_value};
use tuwunel_core::{
	Err, Result, debug_info, debug_warn, err, info,
	matrix::{StateKey, pdu::PduBuilder, room_version},
	utils::{BoolExt, option::OptionExt},
	warn,
};
use tuwunel_service::{Services, appservice::RegistrationInfo, rooms::state::RoomMutexGuard};

use crate::{Ruma, client::utils::invite_check};

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
pub(crate) async fn create_room_route(
	State(services): State<crate::State>,
	body: Ruma<create_room::v3::Request>,
) -> Result<create_room::v3::Response> {
	can_create_room_check(&services, &body).await?;
	can_publish_directory_check(&services, &body).await?;

	// Figure out preset. We need it for preset specific events
	let preset = body
		.preset
		.clone()
		.unwrap_or(match &body.visibility {
			| room::Visibility::Public => RoomPreset::PublicChat,
			| _ => RoomPreset::PrivateChat, // Room visibility should not be custom
		});

	let alias = body
		.room_alias_name
		.as_ref()
		.map_async(|alias| room_alias_check(&services, alias, body.appservice_info.as_ref()));

	// Determine room version
	let (room_version, version_rules) = body
		.room_version
		.as_ref()
		.map_or(Ok(&services.server.config.default_room_version), |version| {
			services
				.server
				.supported_room_version(version)
				.then_ok_or_else(version, || {
					err!(Request(UnsupportedRoomVersion(
						"This server does not support room version {version:?}"
					)))
				})
		})
		.and_then(|version| Ok((version, room_version::rules(version)?)))?;

	// Error on existing alias before committing to creation.
	let alias = alias.await.transpose()?;

	// Increment and hold the counter; the room will sync atomically to clients
	// which is preferable.
	let next_count = services.globals.next_count();

	// 1. Create the create event.
	let (room_id, state_lock) = match version_rules.room_id_format {
		| RoomIdFormatVersion::V1 =>
			create_create_event_legacy(&services, &body, room_version, &version_rules).await?,
		| RoomIdFormatVersion::V2 =>
			create_create_event(&services, &body, &preset, room_version, &version_rules)
				.await
				.map_err(|e| {
					err!(Request(InvalidParam("Error while creating m.room.create event: {e}")))
				})?,
	};

	// 2. Let the room creator join
	let sender_user = body.sender_user();
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(sender_user.to_string(), &RoomMemberEventContent {
				displayname: services.users.displayname(sender_user).await.ok(),
				avatar_url: services.users.avatar_url(sender_user).await.ok(),
				blurhash: services.users.blurhash(sender_user).await.ok(),
				is_direct: Some(body.is_direct),
				..RoomMemberEventContent::new(MembershipState::Join)
			}),
			sender_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 3. Power levels
	let mut users = if !version_rules
		.authorization
		.explicitly_privilege_room_creators
	{
		BTreeMap::from_iter([(sender_user.to_owned(), int!(100))])
	} else {
		BTreeMap::new()
	};

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

			if !version_rules
				.authorization
				.additional_room_creators
			{
				users.insert(invite.clone(), int!(100));
			}
		}
	}

	let power_levels_content = default_power_levels_content(
		&version_rules,
		body.power_level_content_override.as_ref(),
		&body.visibility,
		users,
	)?;

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomPowerLevels,
				content: to_raw_value(&power_levels_content)?,
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
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(String::new(), &RoomCanonicalAliasEventContent {
					alias: Some(room_alias_id.to_owned()),
					alt_aliases: vec![],
				}),
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
	let mut is_encrypted = false;
	for event in &body.initial_state {
		let mut pdu_builder = event
			.deserialize_as_unchecked::<PduBuilder>()
			.map_err(|e| {
				err!(Request(InvalidParam(warn!("Invalid initial state event: {e:?}"))))
			})?;

		debug_info!("Room creation initial state event: {event:?}");

		// client/appservice workaround: if a user sends an initial_state event with a
		// state event in there with the content of literally `{}` (not null or empty
		// string), let's just skip it over and warn.
		if pdu_builder.content.get().eq("{}") {
			debug_warn!("skipping empty initial state event with content of `{{}}`: {event:?}");
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

		if pdu_builder.event_type == TimelineEventType::RoomEncryption {
			is_encrypted = true;
		}

		services
			.timeline
			.build_and_append_pdu(pdu_builder, sender_user, &room_id, &state_lock)
			.boxed()
			.await?;
	}

	if services.config.allow_encryption && !is_encrypted {
		use RoomPreset::*;

		let config = services
			.config
			.encryption_enabled_by_default_for_room_type
			.as_deref()
			.unwrap_or("off");

		let invite = matches!(config, "invite");
		let always = matches!(config, "all" | "invite");
		if always || (invite && matches!(preset, PrivateChat | TrustedPrivateChat)) {
			let algorithm = EventEncryptionAlgorithm::MegolmV1AesSha2;
			let content = RoomEncryptionEventContent::new(algorithm);
			services
				.timeline
				.build_and_append_pdu(
					PduBuilder::state(String::new(), &content),
					sender_user,
					&room_id,
					&state_lock,
				)
				.boxed()
				.await?;
		}
	}

	// 7. Events implied by name and topic
	if let Some(name) = &body.name {
		services
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
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(String::new(), &RoomTopicEventContent {
					topic: topic.clone(),
					topic_block: TopicContentBlock::default(),
				}),
				sender_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;
	}

	drop(next_count);
	drop(state_lock);

	// if inviting anyone with room creation and invite check passes
	if (!body.invite.is_empty() || !body.invite_3pid.is_empty())
		&& invite_check(&services, sender_user, &room_id)
			.await
			.is_ok()
	{
		// 8. Events implied by invite (and TODO: invite_3pid)
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

			if let Err(e) = services
				.membership
				.invite(sender_user, user_id, &room_id, None, body.is_direct)
				.boxed()
				.await
			{
				warn!(%e, "Failed to send invite");
			}
		}
	}

	// Homeserver specific stuff
	if let Some(alias) = alias {
		services
			.alias
			.set_alias(&alias, &room_id, sender_user)?;
	}

	if body.visibility == room::Visibility::Public {
		services.directory.set_public(&room_id);

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

async fn create_create_event(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
	preset: &RoomPreset,
	room_version: &RoomVersionId,
	version_rules: &RoomVersionRules,
) -> Result<(OwnedRoomId, RoomMutexGuard)> {
	let _sender_user = body.sender_user();

	let mut create_content = match &body.creation_content {
		| Some(content) => {
			let mut content = content
				.deserialize_as_unchecked::<CanonicalJsonObject>()
				.map_err(|e| {
					err!(Request(BadJson(error!(
						"Failed to deserialise content as canonical JSON: {e}"
					))))
				})?;

			if !services.config.federate_created_rooms
				&& (!services.config.allow_federation || !content.contains_key("m.federate"))
			{
				content.insert("m.federate".into(), json!(false).try_into()?);
			}

			content.insert(
				"room_version".into(),
				json!(room_version.as_str())
					.try_into()
					.map_err(|e| err!(Request(BadJson("Invalid creation content: {e}"))))?,
			);

			content
		},
		| None => {
			let content = RoomCreateEventContent::new_v11();

			let mut content =
				serde_json::from_str::<CanonicalJsonObject>(to_raw_value(&content)?.get())?;

			if !services.config.federate_created_rooms {
				content.insert("m.federate".into(), json!(false).try_into()?);
			}

			content.insert("room_version".into(), json!(room_version.as_str()).try_into()?);
			content
		},
	};

	if version_rules
		.authorization
		.additional_room_creators
	{
		let mut additional_creators = body
			.creation_content
			.as_ref()
			.and_then(|c| {
				c.deserialize_as_unchecked::<CreationContent>()
					.ok()
			})
			.unwrap_or_default()
			.additional_creators;

		if *preset == RoomPreset::TrustedPrivateChat {
			additional_creators.extend(body.invite.clone());
		}

		additional_creators.sort();
		additional_creators.dedup();
		if !additional_creators.is_empty() {
			create_content
				.insert("additional_creators".into(), json!(additional_creators).try_into()?);
		}
	}

	// 1. The room create event, using a placeholder room_id
	let room_id = ruma::room_id!("!thiswillbereplaced").to_owned();
	let state_lock = services.state.mutex.lock(&room_id).await;
	let create_event_id = services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomCreate,
				content: to_raw_value(&create_content)?,
				state_key: Some(StateKey::new()),
				..Default::default()
			},
			body.sender_user(),
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	drop(state_lock);

	// The real room_id is now the event_id.
	let room_id = OwnedRoomId::from_parts('!', create_event_id.localpart(), None)?;
	let state_lock = services.state.mutex.lock(&room_id).await;

	Ok((room_id, state_lock))
}

async fn create_create_event_legacy(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
	room_version: &RoomVersionId,
	_version_rules: &RoomVersionRules,
) -> Result<(OwnedRoomId, RoomMutexGuard)> {
	let room_id: OwnedRoomId = match &body.room_id {
		| None => RoomId::new_v1(&services.server.name),
		| Some(custom_id) => custom_room_id_check(services, custom_id).await?,
	};

	let state_lock = services.state.mutex.lock(&room_id).await;

	let _short_id = services
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	let create_content = match &body.creation_content {
		| Some(content) => {
			use RoomVersionId::*;

			let mut content = content
				.deserialize_as_unchecked::<CanonicalJsonObject>()
				.map_err(|e| {
					err!(Request(BadJson(error!(
						"Failed to deserialise content as canonical JSON: {e}"
					))))
				})?;

			match room_version {
				| V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 => {
					content.insert(
						"creator".into(),
						json!(body.sender_user())
							.try_into()
							.map_err(|e| {
								err!(Request(BadJson(debug_error!(
									"Invalid creation content: {e}"
								))))
							})?,
					);
				},
				| _ => {
					// V11+ removed the "creator" key
				},
			}

			if !services.config.federate_created_rooms
				&& (!services.config.allow_federation || !content.contains_key("m.federate"))
			{
				content.insert("m.federate".into(), json!(false).try_into()?);
			}

			content.insert(
				"room_version".into(),
				json!(room_version.as_str())
					.try_into()
					.map_err(|e| err!(Request(BadJson("Invalid creation content: {e}"))))?,
			);

			content
		},
		| None => {
			use RoomVersionId::*;

			let content = match room_version {
				| V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 =>
					RoomCreateEventContent::new_v1(body.sender_user().to_owned()),
				| _ => RoomCreateEventContent::new_v11(),
			};

			let mut content =
				serde_json::from_str::<CanonicalJsonObject>(to_raw_value(&content)?.get())?;

			if !services.config.federate_created_rooms {
				content.insert("m.federate".into(), json!(false).try_into()?);
			}

			content.insert("room_version".into(), json!(room_version.as_str()).try_into()?);
			content
		},
	};

	// 1. The room create event
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomCreate,
				content: to_raw_value(&create_content)?,
				state_key: Some(StateKey::new()),
				..Default::default()
			},
			body.sender_user(),
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	Ok((room_id, state_lock))
}

/// creates the power_levels_content for the PDU builder
fn default_power_levels_content(
	version_rules: &RoomVersionRules,
	power_level_content_override: Option<&Raw<RoomPowerLevelsEventContent>>,
	visibility: &room::Visibility,
	users: BTreeMap<OwnedUserId, Int>,
) -> Result<serde_json::Value> {
	use serde_json::to_value;

	let mut power_levels_content = RoomPowerLevelsEventContent::new(&version_rules.authorization);
	power_levels_content.users = users;

	let mut power_levels_content = to_value(power_levels_content)?;

	// secure proper defaults of sensitive/dangerous permissions that moderators
	// (power level 50) should not have easy access to
	power_levels_content["events"]["m.room.power_levels"] = to_value(100)?;
	power_levels_content["events"]["m.room.server_acl"] = to_value(100)?;
	power_levels_content["events"]["m.room.encryption"] = to_value(100)?;
	power_levels_content["events"]["m.room.history_visibility"] = to_value(100)?;

	if version_rules
		.authorization
		.explicitly_privilege_room_creators
	{
		power_levels_content["events"]["m.room.tombstone"] = to_value(150)?;
	} else {
		power_levels_content["events"]["m.room.tombstone"] = to_value(100)?;
	}

	// always allow users to respond (not post new) to polls. this is primarily
	// useful in read-only announcement rooms that post a public poll.
	power_levels_content["events"]["org.matrix.msc3381.poll.response"] = to_value(0)?;
	power_levels_content["events"]["m.poll.response"] = to_value(0)?;

	// synapse does this too. clients do not expose these permissions. it prevents
	// default users from calling public rooms, for obvious reasons.
	if *visibility == room::Visibility::Public {
		power_levels_content["events"]["m.call.invite"] = to_value(50)?;
		power_levels_content["events"]["m.call"] = to_value(50)?;
		power_levels_content["events"]["m.call.member"] = to_value(50)?;
		power_levels_content["events"]["org.matrix.msc3401.call"] = to_value(50)?;
		power_levels_content["events"]["org.matrix.msc3401.call.member"] = to_value(50)?;
	}

	if let Some(power_level_content_override) = power_level_content_override {
		let json: JsonObject = serde_json::from_str(power_level_content_override.json().get())
			.map_err(|e| err!(Request(BadJson("Invalid power_level_content_override: {e:?}"))))?;

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
		return Err!(Request(InvalidParam(
			"Room alias contained `:` which is not allowed. Please note that this expects a \
			 localpart, not the full room alias.",
		)));
	} else if room_alias_name.contains(char::is_whitespace) {
		return Err!(Request(InvalidParam(
			"Room alias contained spaces which is not a valid room alias.",
		)));
	}

	// check if room alias is forbidden
	if services
		.config
		.forbidden_alias_names
		.is_match(room_alias_name)
	{
		return Err!(Request(Unknown("Room alias name is forbidden.")));
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
		.alias
		.resolve_local_alias(&full_room_alias)
		.await
		.is_ok()
	{
		return Err!(Request(RoomInUse("Room alias already exists.")));
	}

	if let Some(info) = appservice_info {
		if !info.aliases.is_match(full_room_alias.as_str()) {
			return Err!(Request(Exclusive("Room alias is not in namespace.")));
		}
	} else if services
		.appservice
		.is_exclusive_alias(&full_room_alias)
		.await
	{
		return Err!(Request(Exclusive("Room alias reserved by appservice.",)));
	}

	debug_info!("Full room alias: {full_room_alias}");

	Ok(full_room_alias)
}

/// if a room is being created with a custom room ID, run our checks against it
async fn custom_room_id_check(services: &Services, custom_room_id: &str) -> Result<OwnedRoomId> {
	// apply forbidden room alias checks to custom room IDs too
	if services
		.config
		.forbidden_alias_names
		.is_match(custom_room_id)
	{
		return Err!(Request(Unknown("Custom room ID is forbidden.")));
	}

	if custom_room_id.contains(':') {
		return Err!(Request(InvalidParam(
			"Custom room ID contained `:` which is not allowed. Please note that this expects a \
			 localpart, not the full room ID.",
		)));
	} else if custom_room_id.contains(char::is_whitespace) {
		return Err!(Request(InvalidParam(
			"Custom room ID contained spaces which is not valid."
		)));
	}

	let server_name = services.globals.server_name();
	let full_room_id = format!("!{custom_room_id}:{server_name}");

	let room_id = OwnedRoomId::parse(full_room_id)
		.inspect(|full_room_id| debug_info!(?full_room_id, "Full custom room ID"))
		.inspect_err(|e| {
			warn!(?e, ?custom_room_id, "Failed to create room with custom room ID");
		})?;

	// check if room ID doesn't already exist instead of erroring on auth check
	if services
		.short
		.get_shortroomid(&room_id)
		.await
		.is_ok()
	{
		return Err!(Request(RoomInUse("Room with that custom room ID already exists",)));
	}

	Ok(room_id)
}

async fn can_publish_directory_check(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
) -> Result {
	if !services
		.server
		.config
		.lockdown_public_room_directory
		|| body.appservice_info.is_some()
		|| body.visibility != room::Visibility::Public
		|| services.users.is_admin(body.sender_user()).await
	{
		return Ok(());
	}

	let msg = format!(
		"Non-admin user {} tried to publish new to the directory while \
		 lockdown_public_room_directory is enabled",
		body.sender_user(),
	);

	warn!("{msg}");
	if services.server.config.admin_room_notices {
		services.admin.notice(&msg).await;
	}

	Err!(Request(Forbidden("Publishing rooms to the room directory is not allowed")))
}

async fn can_create_room_check(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
) -> Result {
	if !services.config.allow_room_creation
		&& body.appservice_info.is_none()
		&& !services.users.is_admin(body.sender_user()).await
	{
		return Err!(Request(Forbidden("Room creation has been disabled.",)));
	}

	Ok(())
}
