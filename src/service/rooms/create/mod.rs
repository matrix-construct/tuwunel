use std::{collections::BTreeMap, sync::Arc};

use futures::FutureExt;
use ruma::{
	CanonicalJsonObject, Int, OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, RoomVersionId,
	UserId,
	api::client::room::create_room::v3::CreationContent,
	events::{
		TimelineEventType,
		room::{
			canonical_alias::RoomCanonicalAliasEventContent,
			create::RoomCreateEventContent,
			guest_access::{GuestAccess, RoomGuestAccessEventContent},
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::RoomJoinRulesEventContent,
			member::{MembershipState, RoomMemberEventContent},
			name::RoomNameEventContent,
			power_levels::RoomPowerLevelsEventContent,
			topic::{RoomTopicEventContent, TopicContentBlock},
		},
	},
	int,
	room::JoinRule,
	room_version_rules::RoomIdFormatVersion,
	serde::{JsonObject, Raw},
};
use serde_json::{json, value::to_raw_value};
use tuwunel_core::{
	Err, Result, err, info,
	matrix::{RoomVersionRules, StateKey, room_version},
	pdu::PduBuilder,
};

use crate::rooms::state::RoomMutexGuard;

pub struct Service {
	services: Arc<crate::OnceServices>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { services: args.services.clone() }))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	pub async fn create_room(
		&self,
		sender_user: &UserId,
		room_id: Option<&RoomId>,
		room_version: Option<&RoomVersionId>,
		alias: Option<&RoomAliasId>,
		additional_creators: &[OwnedUserId],
		is_direct: bool,
		initial_state: Vec<PduBuilder>,
		join_rule: JoinRule,
		guest_access: GuestAccess,
		publish: bool,
		name: Option<&str>,
		topic: Option<&str>,
		power_level_content_override: Option<&Raw<RoomPowerLevelsEventContent>>,
		creation_content: Option<&Raw<CreationContent>>,
	) -> Result<(OwnedRoomId, RoomMutexGuard)> {
		let room_version =
			room_version.unwrap_or(&self.services.server.config.default_room_version);

		if !self
			.services
			.server
			.supported_room_version(room_version)
		{
			return Err!(Request(UnsupportedRoomVersion(
				"This server does not support room version {room_version:?}"
			)));
		}

		let version_rules = room_version::rules(room_version)?;

		// Increment and hold the counter; the room will sync atomically to clients
		// which is preferable.
		let next_count = self.services.globals.next_count();

		// 1. Apply the create event.
		let (room_id, state_lock) = match version_rules.room_id_format {
			| RoomIdFormatVersion::V1 =>
				self.build_create_event_legacy(
					room_id,
					room_version,
					&version_rules,
					sender_user,
					creation_content,
				)
				.await?,
			| RoomIdFormatVersion::V2 => self
				.build_create_event(
					room_version,
					&version_rules,
					additional_creators,
					sender_user,
					creation_content,
				)
				.await
				.map_err(|e| {
					err!(Request(InvalidParam("Error while creating m.room.create event: {e}")))
				})?,
		};

		// 2. Let the room creator join
		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(sender_user.to_string(), &RoomMemberEventContent {
					displayname: self
						.services
						.users
						.displayname(sender_user)
						.await
						.ok(),
					avatar_url: self
						.services
						.users
						.avatar_url(sender_user)
						.await
						.ok(),
					blurhash: self
						.services
						.users
						.blurhash(sender_user)
						.await
						.ok(),
					is_direct: Some(is_direct),
					..RoomMemberEventContent::new(MembershipState::Join)
				}),
				sender_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;

		// 3. Power levels
		let power_levels_content = Self::build_power_levels_content(
			&version_rules,
			power_level_content_override,
			publish,
			sender_user,
			additional_creators,
		)?;

		self.services
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
		if let Some(room_alias_id) = alias {
			self.services
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

		// 5.1 Join Rules
		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(String::new(), &RoomJoinRulesEventContent::new(join_rule)),
				sender_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;

		// 5.2 History Visibility
		self.services
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
		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(String::new(), &RoomGuestAccessEventContent::new(guest_access)),
				sender_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;

		// 6. Events listed in initial_state
		for event in initial_state {
			self.services
				.timeline
				.build_and_append_pdu(event, sender_user, &room_id, &state_lock)
				.boxed()
				.await?;
		}

		// 7. Events implied by name and topic
		if let Some(name) = name {
			self.services
				.timeline
				.build_and_append_pdu(
					PduBuilder::state(String::new(), &RoomNameEventContent::new(name.to_owned())),
					sender_user,
					&room_id,
					&state_lock,
				)
				.boxed()
				.await?;
		}

		if let Some(topic) = topic {
			self.services
				.timeline
				.build_and_append_pdu(
					PduBuilder::state(String::new(), &RoomTopicEventContent {
						topic: topic.to_owned(),
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

		if let Some(alias) = alias {
			self.services
				.alias
				.set_alias(alias, &room_id, sender_user)?;
		}

		if publish {
			self.publish(&room_id, sender_user).await;
		}

		info!("{sender_user} created a room with room ID {room_id}");

		Ok((room_id, state_lock))
	}

	async fn publish(&self, room_id: &RoomId, sender_user: &UserId) {
		self.services.directory.set_public(room_id);

		if self.services.config.admin_room_notices {
			self.services
				.admin
				.send_text(&format!(
					"{sender_user} made {} public to the room directory",
					&room_id
				))
				.await;
		}
		info!("{sender_user} made {0} public to the room directory", &room_id);
	}

	async fn build_create_event_legacy(
		&self,
		room_id: Option<&RoomId>,
		room_version: &RoomVersionId,
		version_rules: &RoomVersionRules,
		sender_user: &UserId,
		creation_content: Option<&Raw<CreationContent>>,
	) -> Result<(OwnedRoomId, RoomMutexGuard)> {
		let room_id = room_id
			.map(ToOwned::to_owned)
			.unwrap_or_else(|| RoomId::new_v1(&self.services.server.name));

		let state_lock = self.services.state.mutex.lock(&room_id).await;

		let _short_id = self
			.services
			.short
			.get_or_create_shortroomid(&room_id)
			.await;

		let create_content = match creation_content {
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
						json!(sender_user).try_into().map_err(|e| {
							err!(Request(BadJson(debug_error!("Invalid creation content: {e}"))))
						})?,
					);
				}

				if !self.services.config.federate_created_rooms {
					if !self.services.config.allow_federation
						|| !content.contains_key("m.federate")
					{
						content.insert("m.federate".into(), json!(false).try_into()?);
					}
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
						RoomCreateEventContent::new_v1(sender_user.to_owned()),
					| _ => RoomCreateEventContent::new_v11(),
				};

				let mut content =
					serde_json::from_str::<CanonicalJsonObject>(to_raw_value(&content)?.get())?;

				if !self.services.config.federate_created_rooms {
					content.insert("m.federate".into(), json!(false).try_into()?);
				}

				content.insert("room_version".into(), json!(room_version.as_str()).try_into()?);
				content
			},
		};

		// 1. The room create event
		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					event_type: TimelineEventType::RoomCreate,
					content: to_raw_value(&create_content)?,
					state_key: Some(StateKey::new()),
					..Default::default()
				},
				sender_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;

		Ok((room_id, state_lock))
	}

	async fn build_create_event(
		&self,
		room_version: &RoomVersionId,
		version_rules: &RoomVersionRules,
		creators: &[OwnedUserId],
		sender_user: &UserId,
		creation_content: Option<&Raw<CreationContent>>,
	) -> Result<(OwnedRoomId, RoomMutexGuard)> {
		let mut create_content = match creation_content {
			| Some(content) => content
				.deserialize_as_unchecked::<CanonicalJsonObject>()
				.map_err(|e| {
					err!(Request(BadJson(error!(
						"Failed to deserialise content as canonical JSON: {e}"
					))))
				})?,
			| None => serde_json::from_str::<CanonicalJsonObject>(
				to_raw_value(&RoomCreateEventContent::new_v11())?.get(),
			)?,
		};

		if !self.services.config.federate_created_rooms {
			if !self.services.config.allow_federation
				|| !create_content.contains_key("m.federate")
			{
				create_content.insert("m.federate".into(), json!(false).try_into()?);
			}
		}

		create_content.insert("room_version".into(), json!(room_version.as_str()).try_into()?);

		if version_rules
			.authorization
			.additional_room_creators
		{
			let mut additional_creators = creation_content
				.and_then(|c| {
					c.deserialize_as_unchecked::<CreationContent>()
						.ok()
				})
				.unwrap_or_default()
				.additional_creators;

			additional_creators.extend_from_slice(creators);

			additional_creators.sort();
			additional_creators.dedup();
			if !additional_creators.is_empty() {
				create_content
					.insert("additional_creators".into(), json!(additional_creators).try_into()?);
			}
		}

		// 1. The room create event, using a placeholder room_id
		let room_id = ruma::room_id!("!thiswillbereplaced").to_owned();
		let state_lock = self.services.state.mutex.lock(&room_id).await;
		let create_event_id = self
			.services
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					event_type: TimelineEventType::RoomCreate,
					content: to_raw_value(&create_content)?,
					state_key: Some(StateKey::new()),
					..Default::default()
				},
				sender_user,
				&room_id,
				&state_lock,
			)
			.boxed()
			.await?;

		drop(state_lock);

		// The real room_id is now the event_id.
		let room_id = OwnedRoomId::from_parts('!', create_event_id.localpart(), None)?;
		let state_lock = self.services.state.mutex.lock(&room_id).await;

		Ok((room_id, state_lock))
	}

	fn build_power_levels_content(
		version_rules: &RoomVersionRules,
		power_level_content_override: Option<&Raw<RoomPowerLevelsEventContent>>,
		publish: bool,
		sender_user: &UserId,
		additional_creators: &[OwnedUserId],
	) -> Result<serde_json::Value> {
		use serde_json::to_value;

		let mut power_levels_content =
			RoomPowerLevelsEventContent::new(&version_rules.authorization);

		let mut users: BTreeMap<OwnedUserId, Int> = BTreeMap::new();

		let creator_power_level = int!(100);

		if !version_rules
			.authorization
			.explicitly_privilege_room_creators
		{
			users.insert(sender_user.to_owned(), creator_power_level);
		}
		if !version_rules
			.authorization
			.additional_room_creators
		{
			for user_id in additional_creators {
				users.insert(user_id.to_owned(), creator_power_level);
			}
		}

		power_levels_content.users = users;

		// secure proper defaults of sensitive/dangerous permissions that moderators
		// (power level 50) should not have easy access to
		power_levels_content
			.events
			.insert(TimelineEventType::RoomPowerLevels, int!(100));
		power_levels_content
			.events
			.insert(TimelineEventType::RoomServerAcl, int!(100));
		power_levels_content
			.events
			.insert(TimelineEventType::RoomEncryption, int!(100));
		power_levels_content
			.events
			.insert(TimelineEventType::RoomHistoryVisibility, int!(100));

		if version_rules
			.authorization
			.explicitly_privilege_room_creators
		{
			power_levels_content
				.events
				.insert(TimelineEventType::RoomTombstone, int!(150));
		} else {
			power_levels_content
				.events
				.insert(TimelineEventType::RoomTombstone, int!(100));
		}

		// always allow users to respond (not post new) to polls. this is primarily
		// useful in read-only announcement rooms that post a public poll.
		power_levels_content
			.events
			.insert(TimelineEventType::PollResponse, int!(0));
		power_levels_content
			.events
			.insert(TimelineEventType::UnstablePollResponse, int!(0));

		// synapse does this too. clients do not expose these permissions. it prevents
		// default users from calling public rooms, for obvious reasons.
		if publish {
			power_levels_content
				.events
				.insert(TimelineEventType::CallInvite, int!(50));
			power_levels_content
				.events
				.insert(TimelineEventType::CallMember, int!(50));
		}

		let mut power_levels_content = to_value(power_levels_content)?;

		if let Some(power_level_content_override) = power_level_content_override {
			let json: JsonObject =
				serde_json::from_str(power_level_content_override.json().get()).map_err(|e| {
					err!(Request(BadJson("Invalid power_level_content_override: {e:?}")))
				})?;

			for (key, value) in json {
				power_levels_content[key] = value;
			}
		}

		Ok(power_levels_content)
	}
}
