mod room_state;
mod server_can;
mod state;
mod user_can;

use std::sync::Arc;

use async_trait::async_trait;
use futures::{FutureExt, TryFutureExt, future::try_join};
use ruma::{
	EventEncryptionAlgorithm, OwnedRoomAliasId, RoomId, UserId,
	events::{
		StateEventType,
		room::{
			avatar::RoomAvatarEventContent,
			canonical_alias::RoomCanonicalAliasEventContent,
			create::RoomCreateEventContent,
			encryption::RoomEncryptionEventContent,
			guest_access::{GuestAccess, RoomGuestAccessEventContent},
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::RoomMemberEventContent,
			name::RoomNameEventContent,
			power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
			topic::RoomTopicEventContent,
		},
	},
	room::RoomType,
};
use tuwunel_core::{
	Result, err,
	matrix::{Pdu, room_version, state_res::events::RoomCreateEvent},
};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { services: args.services.clone() }))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Gets the effective power levels of a room, regardless of if there is an
	/// `m.rooms.power_levels` state.
	pub async fn get_power_levels(&self, room_id: &RoomId) -> Result<RoomPowerLevels> {
		let create = self.get_create(room_id);
		let power_levels = self
			.room_state_get_content(room_id, &StateEventType::RoomPowerLevels, "")
			.map_ok(|c: RoomPowerLevelsEventContent| c)
			.map(Result::ok)
			.map(Ok);

		let (create, power_levels) = try_join(create, power_levels).await?;

		let room_version = create.room_version()?;
		let rules = room_version::rules(&room_version)?;
		let creators = create.creators(&rules.authorization)?;

		Ok(RoomPowerLevels::new(power_levels.into(), &rules.authorization, creators))
	}

	pub async fn get_create(&self, room_id: &RoomId) -> Result<RoomCreateEvent<Pdu>> {
		self.room_state_get(room_id, &StateEventType::RoomCreate, "")
			.await
			.map(RoomCreateEvent::new)
	}

	pub async fn get_name(&self, room_id: &RoomId) -> Result<String> {
		self.room_state_get_content(room_id, &StateEventType::RoomName, "")
			.await
			.map(|c: RoomNameEventContent| c.name)
	}

	pub async fn get_avatar(&self, room_id: &RoomId) -> Result<RoomAvatarEventContent> {
		self.room_state_get_content(room_id, &StateEventType::RoomAvatar, "")
			.await
	}

	pub async fn get_member(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<RoomMemberEventContent> {
		self.room_state_get_content(room_id, &StateEventType::RoomMember, user_id.as_str())
			.await
	}

	/// Checks if guests are able to view room content without joining
	pub async fn is_world_readable(&self, room_id: &RoomId) -> bool {
		self.room_state_get_content(room_id, &StateEventType::RoomHistoryVisibility, "")
			.await
			.map(|c: RoomHistoryVisibilityEventContent| {
				c.history_visibility == HistoryVisibility::WorldReadable
			})
			.unwrap_or(false)
	}

	/// Checks if guests are able to join a given room
	pub async fn guest_can_join(&self, room_id: &RoomId) -> bool {
		self.room_state_get_content(room_id, &StateEventType::RoomGuestAccess, "")
			.await
			.map(|c: RoomGuestAccessEventContent| c.guest_access == GuestAccess::CanJoin)
			.unwrap_or(false)
	}

	/// Gets the primary alias from canonical alias event
	pub async fn get_canonical_alias(&self, room_id: &RoomId) -> Result<OwnedRoomAliasId> {
		self.room_state_get_content(room_id, &StateEventType::RoomCanonicalAlias, "")
			.await
			.and_then(|c: RoomCanonicalAliasEventContent| {
				c.alias
					.ok_or_else(|| err!(Request(NotFound("No alias found in event content."))))
			})
	}

	/// Gets the room topic
	pub async fn get_room_topic(&self, room_id: &RoomId) -> Result<String> {
		self.room_state_get_content(room_id, &StateEventType::RoomTopic, "")
			.await
			.map(|c: RoomTopicEventContent| c.topic)
	}

	/// Returns the join rules for a given room (`JoinRule` type). Will default
	/// to Invite if doesnt exist or invalid
	pub async fn get_join_rules(&self, room_id: &RoomId) -> JoinRule {
		self.room_state_get_content(room_id, &StateEventType::RoomJoinRules, "")
			.await
			.map_or(JoinRule::Invite, |c: RoomJoinRulesEventContent| c.join_rule)
	}

	pub async fn get_room_type(&self, room_id: &RoomId) -> Result<RoomType> {
		self.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
			.await
			.and_then(|content: RoomCreateEventContent| {
				content
					.room_type
					.ok_or_else(|| err!(Request(NotFound("No type found in event content"))))
			})
	}

	/// Gets the room's encryption algorithm if `m.room.encryption` state event
	/// is found
	pub async fn get_room_encryption(
		&self,
		room_id: &RoomId,
	) -> Result<EventEncryptionAlgorithm> {
		self.room_state_get_content(room_id, &StateEventType::RoomEncryption, "")
			.await
			.map(|content: RoomEncryptionEventContent| content.algorithm)
	}

	pub async fn is_encrypted_room(&self, room_id: &RoomId) -> bool {
		self.room_state_get(room_id, &StateEventType::RoomEncryption, "")
			.await
			.is_ok()
	}
}
