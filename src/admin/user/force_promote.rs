use futures::StreamExt;
use ruma::{
	Int, OwnedRoomOrAliasId,
	events::room::power_levels::{RoomPowerLevelsEventContent, UserPowerLevel},
};
use tuwunel_core::{Err, Result, info, matrix::pdu::PduBuilder, utils::ReadyExt};

use crate::{
	admin_command,
	utils::{check_existing_local_user, parse_user_id},
};

#[admin_command]
pub(super) async fn force_promote(
	&self,
	target_id: String,
	room_id: OwnedRoomOrAliasId,
) -> Result {
	let target_id = parse_user_id(self.services, &target_id)?;

	if self.services.globals.user_is_local(&target_id) {
		check_existing_local_user(self.services, &target_id).await?;
	}

	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	let room_power_levels = self
		.services
		.state_accessor
		.get_power_levels(&room_id)
		.await?;

	let privileged_member = self
		.services
		.state_cache
		.room_members(&room_id)
		.ready_filter(|member_id| {
			self.services.globals.user_is_local(member_id)
				&& room_power_levels.user_can_change_user_power_level(member_id, &target_id)
		})
		.map(ToOwned::to_owned)
		.ready_fold_default(|selected_user, member_id| match selected_user {
			| None => Some(member_id),
			| Some(selected_user) => Some(
				if room_power_levels.for_user(&selected_user)
					> room_power_levels.for_user(&member_id)
				{
					selected_user
				} else {
					member_id
				},
			),
		})
		.await;

	let Some(privileged_member) = privileged_member else {
		return Err!("No privileged user exists in room, cannot promote.");
	};

	info!("Selected privileged member {privileged_member}");

	let power_level: Int = match room_power_levels.for_user(&privileged_member) {
		| UserPowerLevel::Infinite => Int::MAX,
		| UserPowerLevel::Int(x) => x,
	};

	let mut power_levels_content: RoomPowerLevelsEventContent = room_power_levels.try_into()?;

	power_levels_content
		.users
		.insert(target_id.clone(), power_level);

	let event_id = self
		.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &power_levels_content),
			&privileged_member,
			&room_id,
			&state_lock,
		)
		.await?;

	drop(state_lock);

	write!(
		self,
		"User {privileged_member} promoted {target_id} to {power_level} power level in \
		 {room_id} - {event_id}"
	)
	.await?;

	Ok(())
}
