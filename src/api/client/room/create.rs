use std::collections::BTreeMap;

use axum::extract::State;
use futures::{FutureExt, StreamExt};
use itertools::Itertools;
use ruma::{
	CanonicalJsonObject, EventEncryptionAlgorithm, Int, OwnedRoomAliasId, OwnedRoomId,
	OwnedUserId, RoomAliasId, RoomId, RoomVersionId, UserId,
	api::client::room::{
		self,
		create_room::{
			self, RoomPowerLevelsContentOverride,
			v3::{CreationContent, RoomPreset},
		},
	},
	events::{
		StateEventType, TimelineEventType,
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
			topic::RoomTopicEventContent,
		},
	},
	int,
	room_version_rules::{RoomIdFormatVersion, RoomVersionRules},
	serde::{JsonObject, Raw},
};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json, value::to_raw_value};
use tuwunel_core::{
	Err, Result, debug_info, debug_warn, err, info,
	matrix::{
		StateKey,
		pdu::{Content, PduBuilder},
		room_version,
	},
	utils::{BoolExt, IterStream, ReadyExt, option::OptionExt},
	warn,
};
use tuwunel_service::{Services, appservice::RegistrationInfo, rooms::state::RoomMutexGuard};

use crate::{Ruma, client::utils::invite_check};

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

	// Determine room version
	let (room_version, version_rules) = body
		.room_version
		.as_ref()
		.map_or(Ok(&services.server.config.default_room_version), |version| {
			services
				.config
				.supported_room_version(version)
				.then_ok_or_else(version, || {
					err!(Request(UnsupportedRoomVersion(
						"This server does not support room version {version:?}"
					)))
				})
		})
		.and_then(|version| Ok((version, room_version::rules(version)?)))?;

	// Error on existing alias before committing to creation.
	let alias = body
		.room_alias_name
		.as_ref()
		.map_async(|alias| room_alias_check(&services, alias, body.appservice_info.as_ref()))
		.await
		.transpose()?;

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

	let sender_user = body.sender_user();

	// 2. Let the room creator join
	apply_creator_join_pdu(&services, &body, sender_user, &room_id, &state_lock)
		.boxed()
		.await?;

	// 3. Power levels
	apply_power_levels_pdu(
		&services,
		&body,
		&preset,
		&version_rules,
		sender_user,
		&room_id,
		&state_lock,
	)
	.boxed()
	.await?;

	// 4. Canonical room alias
	if let Some(room_alias_id) = &alias {
		apply_canonical_alias_pdu(&services, room_alias_id, sender_user, &room_id, &state_lock)
			.boxed()
			.await?;
	}

	// 5. Events set by preset
	let initial_state =
		apply_preset_state_pdus(&services, &body, &preset, sender_user, &room_id, &state_lock)
			.boxed()
			.await?;

	// 6. Events listed in initial_state
	apply_initial_state_pdus(
		&services,
		initial_state,
		&preset,
		sender_user,
		&room_id,
		&state_lock,
	)
	.boxed()
	.await?;

	// 7. Events implied by name and topic
	apply_name_and_topic_pdus(&services, &body, sender_user, &room_id, &state_lock)
		.boxed()
		.await?;

	drop(next_count);
	drop(state_lock);

	// if inviting anyone with room creation and invite check passes
	if (!body.invite.is_empty() || !body.invite_3pid.is_empty())
		&& invite_check(&services, sender_user, &room_id)
			.await
			.is_ok()
	{
		process_invites(&services, &body, sender_user, &room_id)
			.boxed()
			.await;
	}

	finalize_alias_and_directory(&services, &body, alias.as_deref(), sender_user, &room_id)
		.await?;

	info!("{sender_user} created a room with room ID {room_id}");

	Ok(create_room::v3::Response::new(room_id))
}

async fn apply_creator_join_pdu(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
	sender_user: &UserId,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
) -> Result {
	let mut content = RoomMemberEventContent {
		is_direct: body.is_direct,
		..RoomMemberEventContent::new(MembershipState::Join)
	};

	services
		.profile
		.fill_profile_data(sender_user, &mut content)
		.await;

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(sender_user.to_string(), &content),
			sender_user,
			room_id,
			state_lock,
		)
		.await
		.map(|_| ())
}

async fn apply_power_levels_pdu(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
	preset: &RoomPreset,
	version_rules: &RoomVersionRules,
	sender_user: &UserId,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
) -> Result {
	let users =
		build_power_levels_users(services, body, preset, version_rules, sender_user).await;

	let default_override = services
		.config
		.default_power_level_content_override
		.as_ref();

	let power_levels_content = default_power_levels_content(
		version_rules,
		default_override,
		body.power_level_content_override.as_ref(),
		preset,
		users,
	)?;

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomPowerLevels,
				content: to_raw_value(&power_levels_content)?.into(),
				state_key: Some(StateKey::new()),
				..Default::default()
			},
			sender_user,
			room_id,
			state_lock,
		)
		.await
		.map(|_| ())
}

async fn build_power_levels_users(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
	preset: &RoomPreset,
	version_rules: &RoomVersionRules,
	sender_user: &UserId,
) -> BTreeMap<OwnedUserId, Int> {
	let seed = version_rules
		.authorization
		.explicitly_privilege_room_creators
		.or(|| (sender_user.to_owned(), int!(100)))
		.into_iter()
		.collect::<BTreeMap<_, _>>();

	let trusted_invitees = *preset == RoomPreset::TrustedPrivateChat
		&& !version_rules
			.authorization
			.additional_room_creators;

	if !trusted_invitees {
		return seed;
	}

	body.invite
		.iter()
		.stream()
		.filter(|&invite| invite_allowed(services, sender_user, invite))
		.ready_fold(seed, |mut users, invite| {
			users.insert(invite.clone(), int!(100));
			users
		})
		.await
}

async fn apply_canonical_alias_pdu(
	services: &Services,
	room_alias_id: &RoomAliasId,
	sender_user: &UserId,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
) -> Result {
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomCanonicalAliasEventContent {
				alias: Some(room_alias_id.to_owned()),
				alt_aliases: vec![],
			}),
			sender_user,
			room_id,
			state_lock,
		)
		.await
		.map(|_| ())
}

async fn apply_preset_state_pdus(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
	preset: &RoomPreset,
	sender_user: &UserId,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
) -> Result<Vec<InitialEvent>> {
	let mut initial_state = body
		.initial_state
		.iter()
		.map(|state| Ok(state.deserialize_as_unchecked::<InitialEvent>()?))
		.filter_ok(|event| {
			services.config.allow_encryption || event.event_type != StateEventType::RoomEncryption
		})
		.filter_ok(|event| {
			// client/appservice workaround: if a user sends an initial_state event with a
			// state event in there with the content of literally `{}` (not null or empty
			// string), let's just skip it over and warn.
			if event.content.json().get() == "{}" {
				debug_warn!("skipping empty initial state event of type {}", event.event_type);
				false
			} else {
				true
			}
		})
		.filter_ok(|event| body.name.is_none() || event.event_type != StateEventType::RoomName)
		.filter_ok(|event| body.topic.is_none() || event.event_type != StateEventType::RoomTopic)
		.collect::<Result<Vec<_>>>()?;

	let join_rule_pdubuilder =
		take_initial(&mut initial_state, &StateEventType::RoomJoinRules, "")
			.map(Into::into)
			.unwrap_or_else(|| {
				PduBuilder::state(
					String::new(),
					&RoomJoinRulesEventContent::new(match preset {
						| RoomPreset::PublicChat => JoinRule::Public,
						// according to spec "invite" is the default
						| _ => JoinRule::Invite,
					}),
				)
			});

	let history_visibility_pdubuilder =
		take_initial(&mut initial_state, &StateEventType::RoomHistoryVisibility, "")
			.map(Into::into)
			.unwrap_or_else(|| {
				PduBuilder::state(
					String::new(),
					&RoomHistoryVisibilityEventContent::new(HistoryVisibility::Shared),
				)
			});

	let guest_access_pdubuilder =
		take_initial(&mut initial_state, &StateEventType::RoomGuestAccess, "")
			.map(Into::into)
			.unwrap_or_else(|| {
				PduBuilder::state(
					String::new(),
					&RoomGuestAccessEventContent::new(match preset {
						| RoomPreset::PublicChat => GuestAccess::Forbidden,
						| _ => GuestAccess::CanJoin,
					}),
				)
			});

	// 5.1 Join Rules
	services
		.timeline
		.build_and_append_pdu(join_rule_pdubuilder, sender_user, room_id, state_lock)
		.boxed()
		.await?;

	// 5.2 History Visibility
	services
		.timeline
		.build_and_append_pdu(history_visibility_pdubuilder, sender_user, room_id, state_lock)
		.boxed()
		.await?;

	// 5.3 Guest Access
	services
		.timeline
		.build_and_append_pdu(guest_access_pdubuilder, sender_user, room_id, state_lock)
		.boxed()
		.await?;

	Ok(initial_state)
}

async fn apply_initial_state_pdus(
	services: &Services,
	initial_state: Vec<InitialEvent>,
	preset: &RoomPreset,
	sender_user: &UserId,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
) -> Result {
	let is_encrypted = initial_state
		.iter()
		.any(|event| event.event_type == StateEventType::RoomEncryption);

	for event in initial_state {
		services
			.timeline
			.build_and_append_pdu(event.into(), sender_user, room_id, state_lock)
			.boxed()
			.await?;
	}

	if !services.config.allow_encryption || is_encrypted {
		return Ok(());
	}

	let config = services
		.config
		.encryption_enabled_by_default_for_room_type
		.as_deref();

	let should_encrypt = match config {
		| Some("all") => true,
		| Some("invite") =>
			matches!(preset, RoomPreset::PrivateChat | RoomPreset::TrustedPrivateChat),
		| _ => false,
	};

	if !should_encrypt {
		return Ok(());
	}

	let algorithm = EventEncryptionAlgorithm::MegolmV1AesSha2;
	let content = RoomEncryptionEventContent::new(algorithm);
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &content),
			sender_user,
			room_id,
			state_lock,
		)
		.boxed()
		.await?;

	Ok(())
}

async fn apply_name_and_topic_pdus(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
	sender_user: &UserId,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
) -> Result {
	if let Some(name) = &body.name {
		services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(String::new(), &RoomNameEventContent::new(name.clone())),
				sender_user,
				room_id,
				state_lock,
			)
			.boxed()
			.await?;
	}

	if let Some(topic) = &body.topic {
		services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(String::new(), &RoomTopicEventContent::new(topic.clone())),
				sender_user,
				room_id,
				state_lock,
			)
			.boxed()
			.await?;
	}

	Ok(())
}

async fn process_invites(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
	sender_user: &UserId,
	room_id: &RoomId,
) {
	// 8. Events implied by invite (and TODO: invite_3pid)
	body.invite
		.iter()
		.stream()
		.filter(|&user_id| invite_allowed(services, sender_user, user_id))
		.for_each(async |user_id| {
			if let Err(e) = services
				.membership
				.invite(sender_user, user_id, room_id, None, body.is_direct)
				.boxed()
				.await
			{
				warn!(%e, "Failed to send invite");
			}
		})
		.await;
}

/// Gate an invitee against the sender's ignore list, the recipient's ignore
/// list, and MSC4380 `m.invite_permission_config`.
async fn invite_allowed(services: &Services, sender_user: &UserId, invitee: &UserId) -> bool {
	!(services
		.users
		.user_is_ignored(sender_user, invitee)
		.await || services
		.users
		.user_is_ignored(invitee, sender_user)
		.await || services.users.invites_blocked(invitee).await)
}

async fn finalize_alias_and_directory(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
	alias: Option<&RoomAliasId>,
	sender_user: &UserId,
	room_id: &RoomId,
) -> Result {
	if let Some(alias) = alias {
		services
			.alias
			.set_alias_by(alias, room_id, sender_user)?;
	}

	if body.visibility == room::Visibility::Public {
		services.directory.set_public(room_id);

		services
			.admin
			.notify_loud(&format!("{sender_user} made {room_id} public to the room directory"))
			.await;

		info!("{sender_user} made {0} public to the room directory", room_id);
	}

	Ok(())
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
				content: to_raw_value(&create_content)?.into(),
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
	version_rules: &RoomVersionRules,
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
			let mut content = content
				.deserialize_as_unchecked::<CanonicalJsonObject>()
				.map_err(|e| {
					err!(Request(BadJson(error!(
						"Failed to deserialise content as canonical JSON: {e}"
					))))
				})?;

			if !version_rules.authorization.use_room_create_sender {
				content.insert(
					"creator".into(),
					json!(body.sender_user())
						.try_into()
						.map_err(|e| {
							err!(Request(BadJson(debug_error!("Invalid creation content: {e}"))))
						})?,
				);
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
			let content = if !version_rules.authorization.use_room_create_sender {
				RoomCreateEventContent::new_v1(body.sender_user().to_owned())
			} else {
				RoomCreateEventContent::new_v11()
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
				content: to_raw_value(&create_content)?.into(),
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
	default_power_level_content_override: Option<&JsonValue>,
	power_level_content_override: Option<&Raw<RoomPowerLevelsContentOverride>>,
	preset: &RoomPreset,
	users: BTreeMap<OwnedUserId, Int>,
) -> Result<JsonValue> {
	use serde_json::to_value;

	let mut power_levels_content = RoomPowerLevelsEventContent::new(&version_rules.authorization);
	power_levels_content.users = users;

	let mut power_levels_content = to_value(power_levels_content)?;

	// secure proper defaults of sensitive/dangerous permissions that moderators
	// (power level 50) should not have easy access to
	power_levels_content["events"]["m.room.power_levels"] = json!(100);
	power_levels_content["events"]["m.room.server_acl"] = json!(100);
	power_levels_content["events"]["m.room.encryption"] = json!(100);
	power_levels_content["events"]["m.room.history_visibility"] = json!(100);

	if version_rules
		.authorization
		.explicitly_privilege_room_creators
	{
		power_levels_content["events"]["m.room.tombstone"] = json!(150);
	} else {
		power_levels_content["events"]["m.room.tombstone"] = json!(100);
	}

	// always allow users to respond (not post new) to polls. this is primarily
	// useful in read-only announcement rooms that post a public poll.
	power_levels_content["events"]["org.matrix.msc3381.poll.response"] = json!(0);
	power_levels_content["events"]["m.poll.response"] = json!(0);

	// public_chat: pin invite and call-setup events at PL 50. Synapse pins
	// invite and m.call.invite here; the MSC3401 entries are tuwunel-only.
	if *preset == RoomPreset::PublicChat {
		power_levels_content["invite"] = json!(50);
		power_levels_content["events"]["m.call.invite"] = json!(50);
		power_levels_content["events"]["m.call"] = json!(50);
		power_levels_content["events"]["m.call.member"] = json!(50);
		power_levels_content["events"]["org.matrix.msc3401.call"] = json!(50);
		power_levels_content["events"]["org.matrix.msc3401.call.member"] = json!(50);
	}

	if let Some(default_power_level_content_override) = default_power_level_content_override {
		let overrides = default_power_level_content_override
			.as_object()
			.expect("default_power_level_content_override is validated at startup")
			.iter()
			.map(|(key, value)| (key.clone(), value.clone()));

		merge_power_level_content_override(&mut power_levels_content, overrides);
	}

	if let Some(power_level_content_override) = power_level_content_override {
		let overrides: JsonObject =
			serde_json::from_str(power_level_content_override.json().get()).map_err(|e| {
				err!(Request(BadJson("Invalid power_level_content_override: {e:?}")))
			})?;

		merge_power_level_content_override(&mut power_levels_content, overrides);
	}

	Ok(power_levels_content)
}

/// Replace each top-level power-levels key wholesale; no deep merge.
fn merge_power_level_content_override(
	power_levels_content: &mut JsonValue,
	overrides: impl IntoIterator<Item = (String, JsonValue)>,
) {
	power_levels_content
		.as_object_mut()
		.expect("power levels content must serialize to an object")
		.extend(overrides);
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
		|| services
			.admin
			.user_is_admin(body.sender_user())
			.await
	{
		return Ok(());
	}

	let msg = format!(
		"Non-admin user {} tried to publish new to the directory while \
		 lockdown_public_room_directory is enabled",
		body.sender_user(),
	);

	warn!("{msg}");
	services.admin.notify(&msg).await;

	Err!(Request(Forbidden("Publishing rooms to the room directory is not allowed")))
}

async fn can_create_room_check(
	services: &Services,
	body: &Ruma<create_room::v3::Request>,
) -> Result {
	if !services.config.allow_room_creation
		&& body.appservice_info.is_none()
		&& !services
			.admin
			.user_is_admin(body.sender_user())
			.await
	{
		return Err!(Request(Forbidden("Room creation has been disabled.",)));
	}

	Ok(())
}

#[derive(Deserialize)]
struct InitialEvent {
	#[serde(rename = "type")]
	event_type: StateEventType,

	#[serde(default = "StateKey::new")]
	state_key: StateKey,

	content: Content,
}

impl From<InitialEvent> for PduBuilder {
	fn from(value: InitialEvent) -> Self {
		Self {
			event_type: value.event_type.into(),
			content: value.content,
			unsigned: None,
			state_key: Some(value.state_key),
			redacts: None,
			timestamp: None,
		}
	}
}

fn take_initial(
	initial_state: &mut Vec<InitialEvent>,
	event_type: &StateEventType,
	state_key: &str,
) -> Option<InitialEvent> {
	initial_state
		.extract_if(.., |event| &event.event_type == event_type && event.state_key == state_key)
		.next()
}

#[cfg(test)]
mod tests {
	use tuwunel_core::matrix::room_version::rules;

	use super::*;

	#[test]
	fn default_power_levels_content_applies_server_default_override() {
		let version_rules = rules(&RoomVersionId::V11).expect("supported room version");

		let content = default_power_levels_content(
			&version_rules,
			Some(&json!({ "users_default": 50 })),
			None,
			&RoomPreset::PrivateChat,
			BTreeMap::new(),
		)
		.expect("power levels content");

		assert_eq!(content["users_default"], json!(50));
	}

	#[test]
	fn request_override_wins_over_server_default_override() {
		let version_rules = rules(&RoomVersionId::V11).expect("supported room version");
		let request_override =
			Raw::from_json(to_raw_value(&json!({ "users_default": 75 })).expect("raw json"));

		let content = default_power_levels_content(
			&version_rules,
			Some(&json!({ "users_default": 50 })),
			Some(&request_override),
			&RoomPreset::PrivateChat,
			BTreeMap::new(),
		)
		.expect("power levels content");

		assert_eq!(content["users_default"], json!(75));
	}

	#[test]
	fn default_override_preserves_explicit_user_power_levels() {
		let version_rules = rules(&RoomVersionId::V11).expect("supported room version");
		let creator = OwnedUserId::try_from("@alice:example.com").expect("valid user id");
		let users = BTreeMap::from([(creator.clone(), int!(100))]);

		let content = default_power_levels_content(
			&version_rules,
			Some(&json!({ "users_default": 50 })),
			None,
			&RoomPreset::PrivateChat,
			users,
		)
		.expect("power levels content");

		assert_eq!(content["users_default"], json!(50));
		assert_eq!(content["users"][creator.as_str()], json!(100));
	}
}
