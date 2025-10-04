use std::sync::Arc;

use futures::{FutureExt, StreamExt};
use ruma::{
	OwnedRoomId, UserId,
	events::{StateEventType, room::power_levels::RoomPowerLevelsEventContent},
};
use tuwunel_core::{Event, Result, info, pdu::PduBuilder, utils::ReadyExt, warn};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { services: args.services.clone() }))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Runs through all the deactivation steps:
	///
	/// - Mark as deactivated
	/// - Removing display name
	/// - Removing avatar URL and blurhash
	/// - Removing all profile data
	/// - Leaving all rooms (and forgets all of them)
	pub async fn full_deactivate(&self, user_id: &UserId) -> Result {
		self.services
			.users
			.deactivate_account(user_id)
			.await?;

		let all_joined_rooms: Vec<OwnedRoomId> = self
			.services
			.state_cache
			.rooms_joined(user_id)
			.map(Into::into)
			.collect()
			.await;

		self.services
			.users
			.update_displayname(user_id, None, &all_joined_rooms)
			.await;
		self.services
			.users
			.update_avatar_url(user_id, None, None, &all_joined_rooms)
			.await;

		self.services
			.users
			.all_profile_keys(user_id)
			.ready_for_each(|(profile_key, _)| {
				self.services
					.users
					.set_profile_key(user_id, &profile_key, None);
			})
			.await;

		for room_id in all_joined_rooms {
			let state_lock = self.services.state.mutex.lock(&room_id).await;

			let room_power_levels = self
				.services
				.state_accessor
				.get_power_levels(&room_id)
				.await
				.ok();

			let user_can_change_self = room_power_levels
				.as_ref()
				.is_some_and(|power_levels| {
					power_levels.user_can_change_user_power_level(user_id, user_id)
				});

			let user_can_demote_self = user_can_change_self
				|| self
					.services
					.state_accessor
					.room_state_get(&room_id, &StateEventType::RoomCreate, "")
					.await
					.is_ok_and(|event| event.sender() == user_id);

			if user_can_demote_self {
				let mut power_levels_content: RoomPowerLevelsEventContent = room_power_levels
					.map(TryInto::try_into)
					.transpose()?
					.unwrap_or_default();

				power_levels_content.users.remove(user_id);

				// ignore errors so deactivation doesn't fail
				match self
					.services
					.timeline
					.build_and_append_pdu(
						PduBuilder::state(String::new(), &power_levels_content),
						user_id,
						&room_id,
						&state_lock,
					)
					.await
				{
					| Err(e) => {
						warn!(%room_id, %user_id, "Failed to demote user's own power level: {e}");
					},
					| _ => {
						info!("Demoted {user_id} in {room_id} as part of account deactivation");
					},
				}
			}
		}

		let rooms_joined = self
			.services
			.state_cache
			.rooms_joined(user_id)
			.map(ToOwned::to_owned);

		let rooms_invited = self
			.services
			.state_cache
			.rooms_invited(user_id)
			.map(ToOwned::to_owned);

		let rooms_knocked = self
			.services
			.state_cache
			.rooms_knocked(user_id)
			.map(ToOwned::to_owned);

		let all_rooms: Vec<_> = rooms_joined
			.chain(rooms_invited)
			.chain(rooms_knocked)
			.collect()
			.await;

		for room_id in all_rooms {
			let state_lock = self.services.state.mutex.lock(&room_id).await;

			// ignore errors
			if let Err(e) = self
				.services
				.membership
				.leave(user_id, &room_id, None, false, &state_lock)
				.boxed()
				.await
			{
				warn!(%user_id, "Failed to leave {room_id} remotely: {e}");
			}

			drop(state_lock);

			self.services
				.state_cache
				.forget(&room_id, user_id);
		}

		Ok(())
	}
}
