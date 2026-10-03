use ruma::{
	OwnedRoomOrAliasId,
	events::{
		StateEventType,
		room::power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
	},
};
use tuwunel_core::{Err, Result};
use tuwunel_matrix::{Event, pdu::PduBuilder};

use crate::{admin_command, utils::parse_local_user_id};

#[admin_command]
pub(super) async fn force_demote(&self, user_id: String, room_id: OwnedRoomOrAliasId) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;
	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	assert!(
		self.services.globals.user_is_local(&user_id),
		"Parsed user_id must be a local user"
	);

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	let room_power_levels: Option<RoomPowerLevels> = self
		.services
		.state_accessor
		.get_power_levels(&room_id)
		.await
		.ok();

	let user_can_change_self = room_power_levels
		.as_ref()
		.is_some_and(|power_levels| {
			power_levels.user_can_change_user_power_level(&user_id, &user_id)
		});

	let user_can_demote_self = user_can_change_self
		|| self
			.services
			.state_accessor
			.room_state_get(&room_id, &StateEventType::RoomCreate, "")
			.await
			.is_ok_and(|event| event.sender() == user_id);

	if !user_can_demote_self {
		return Err!("User is not allowed to modify their own power levels in the room.");
	}

	let mut power_levels_content: RoomPowerLevelsEventContent = room_power_levels
		.map(TryInto::try_into)
		.transpose()?
		.unwrap_or_default();

	power_levels_content.users.remove(&user_id);

	let event_id = self
		.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &power_levels_content),
			&user_id,
			&room_id,
			&state_lock,
		)
		.await?;

	write!(
		self,
		"User {user_id} demoted themselves to the room default power level in {room_id} - \
		 {event_id}"
	)
	.await
}
