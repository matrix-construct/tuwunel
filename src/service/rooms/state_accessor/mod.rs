//! Reads room-state snapshots and evaluates state-based access policy.
//!
//! The service resolves current and historical state into typed events. It also
//! centralizes visibility, redaction, erasure, invite, and tombstone decisions.

mod erased;
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
	Result, err, implement,
	matrix::{Pdu, room_version},
	utils::BoolExt,
};

use crate::rooms::state_res::events::RoomCreateEvent;

/// Resolves room state and answers state-based authorization questions.
///
/// Accessors share the state, timeline, short-ID, and membership services so
/// callers use one interpretation of current and historical room state.
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
	/// Returns the effective power levels for a room.
	///
	/// A missing or invalid `m.room.power_levels` event falls back to the room
	/// version's defaults. The create event and its room-version rules are required.
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

	/// Returns the room's current create event wrapper.
	///
	/// The lookup uses the empty state key and returns an error when the event or
	/// its state snapshot cannot be resolved.
	pub async fn get_create(&self, room_id: &RoomId) -> Result<RoomCreateEvent<Pdu>> {
		self.room_state_get(room_id, &StateEventType::RoomCreate, "")
			.await
			.map(RoomCreateEvent::new)
	}

	/// Returns the room's current non-empty name.
	///
	/// Missing, invalid, and empty `m.room.name` content is reported as an error.
	pub async fn get_name(&self, room_id: &RoomId) -> Result<String> {
		self.room_state_get_content(room_id, &StateEventType::RoomName, "")
			.await
			.and_then(|c: RoomNameEventContent| {
				c.name
					.is_empty()
					.is_false()
					.then_some(c.name)
					.ok_or_else(|| err!(Request(NotFound("Empty name found in event content."))))
			})
	}

	/// Returns the room's current avatar content.
	///
	/// Missing or invalid `m.room.avatar` state is returned as an error.
	pub async fn get_avatar(&self, room_id: &RoomId) -> Result<RoomAvatarEventContent> {
		self.room_state_get_content(room_id, &StateEventType::RoomAvatar, "")
			.await
	}

	/// Returns a user's current membership event content in a room.
	///
	/// The user ID is used as the membership state key. Missing or invalid state
	/// is returned as an error.
	pub async fn get_member(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<RoomMemberEventContent> {
		self.room_state_get_content(room_id, &StateEventType::RoomMember, user_id.as_str())
			.await
	}

	/// Reports whether the room is world-readable.
	///
	/// Missing, unreadable, or invalid history-visibility state is treated as not
	/// world-readable.
	pub async fn is_world_readable(&self, room_id: &RoomId) -> bool {
		self.room_state_get_content(room_id, &StateEventType::RoomHistoryVisibility, "")
			.await
			.map(|c: RoomHistoryVisibilityEventContent| {
				c.history_visibility == HistoryVisibility::WorldReadable
			})
			.unwrap_or(false)
	}

	/// Reports whether guest users may join the room.
	///
	/// Missing, unreadable, or invalid guest-access state is treated as denying
	/// guest joins.
	pub async fn guest_can_join(&self, room_id: &RoomId) -> bool {
		self.room_state_get_content(room_id, &StateEventType::RoomGuestAccess, "")
			.await
			.map(|c: RoomGuestAccessEventContent| c.guest_access == GuestAccess::CanJoin)
			.unwrap_or(false)
	}

	/// Returns the room's current primary canonical alias.
	///
	/// Alternate aliases are not considered. Missing state, invalid content, or
	/// an absent primary alias is returned as an error.
	pub async fn get_canonical_alias(&self, room_id: &RoomId) -> Result<OwnedRoomAliasId> {
		self.room_state_get_content(room_id, &StateEventType::RoomCanonicalAlias, "")
			.await
			.and_then(|c: RoomCanonicalAliasEventContent| {
				c.alias
					.ok_or_else(|| err!(Request(NotFound("No alias found in event content."))))
			})
	}

	/// Returns the room's current plain-text topic.
	///
	/// Rich-topic plain text takes precedence over the legacy field. Missing,
	/// invalid, or empty topic content is returned as an error.
	pub async fn get_room_topic(&self, room_id: &RoomId) -> Result<String> {
		self.room_state_get_content(room_id, &StateEventType::RoomTopic, "")
			.await
			.and_then(|content: RoomTopicEventContent| {
				plain_text_topic(content)
					.ok_or_else(|| err!(Request(NotFound("Empty topic found in event content."))))
			})
	}

	/// Returns the room's current join rule.
	///
	/// Any missing, unreadable, or invalid join-rules state falls back to
	/// [`JoinRule::Invite`].
	pub async fn get_join_rules(&self, room_id: &RoomId) -> JoinRule {
		self.room_state_get_content(room_id, &StateEventType::RoomJoinRules, "")
			.await
			.map_or(JoinRule::Invite, |c: RoomJoinRulesEventContent| c.join_rule)
	}

	/// Returns the room type declared by the current create event.
	///
	/// A missing create event, invalid content, or absent room type is returned as
	/// an error; ordinary rooms therefore do not yield a synthetic type.
	pub async fn get_room_type(&self, room_id: &RoomId) -> Result<RoomType> {
		self.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
			.await
			.and_then(|content: RoomCreateEventContent| {
				content
					.room_type
					.ok_or_else(|| err!(Request(NotFound("No type found in event content"))))
			})
	}

	/// Returns the room's configured encryption algorithm.
	///
	/// Missing or invalid `m.room.encryption` state is returned as an error.
	pub async fn get_room_encryption(
		&self,
		room_id: &RoomId,
	) -> Result<EventEncryptionAlgorithm> {
		self.room_state_get_content(room_id, &StateEventType::RoomEncryption, "")
			.await
			.map(|content: RoomEncryptionEventContent| content.algorithm)
	}

	/// Reports whether an encryption state event is present.
	///
	/// This checks that the event can be loaded, but does not deserialize its
	/// content or validate an encryption algorithm.
	pub async fn is_encrypted_room(&self, room_id: &RoomId) -> bool {
		self.room_state_get(room_id, &StateEventType::RoomEncryption, "")
			.await
			.is_ok()
	}
}

/// Checks whether the room federates, per `m.federate` in its create event.
///
/// An absent `m.federate` means the room federates, which is the spec
/// default. A missing or unparsable create event reports the same, so a
/// failed read never reports a room as non-federating.
#[implement(Service)]
pub async fn is_federating(&self, room_id: &RoomId) -> bool {
	self.get_create(room_id)
		.await
		.and_then(|create| create.federate())
		.unwrap_or(true)
}

/// Resolves room-topic content to a non-empty plain-text rendering.
///
/// The `m.topic` block's `text/plain` representation takes precedence under
/// MSC3765, followed by the legacy `topic` field. Empty values yield `None`.
pub(crate) fn plain_text_topic(content: RoomTopicEventContent) -> Option<String> {
	let topic = content
		.topic_block
		.text
		.find_plain()
		.map(ToOwned::to_owned)
		.unwrap_or(content.topic);

	topic.is_empty().is_false().then_some(topic)
}
