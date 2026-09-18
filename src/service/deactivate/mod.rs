use std::sync::Arc;

use futures::{Stream, StreamExt, TryFutureExt, TryStreamExt, future::join};
use ruma::{
	OwnedRoomId, RoomId, UserId,
	events::{
		StateEventType,
		room::{member::MembershipState, power_levels::RoomPowerLevelsEventContent},
	},
};
use tuwunel_core::{
	Event, Result, implement, info,
	pdu::PduBuilder,
	utils::{IterStream, future::TryExtExt, stream::BroadbandExt},
	warn,
};

const CURRENT_MEMBERSHIPS: &[MembershipState] =
	&[MembershipState::Join, MembershipState::Invite, MembershipState::Knock];

// Eight bounds remote leave fanout independently from the storage-tuned stream width.
const LEAVE_CONCURRENCY: usize = 8;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { services: args.services.clone() }))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Deactivates an account, clears its profile, and leaves and forgets its rooms.
///
/// Joined rooms are demoted before leaving. When `erase` is true, MSC4025
/// erasure also marks the user erased and removes contact identifiers and
/// global and room account data before leaving rooms.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn full_deactivate(&self, user_id: &UserId, erase: bool) -> Result {
	self.services
		.users
		.deactivate_account(user_id)
		.await?;

	self.clear_profile(user_id).await;
	self.demote_joined_rooms(user_id).await?;

	if erase {
		let rooms: Vec<_> = self.membership_rooms(user_id).collect().await;

		self.erase_user_data(user_id, &rooms).await;
		self.leave_rooms(user_id, rooms.into_iter().stream())
			.await;
	} else {
		self.leave_rooms(user_id, self.membership_rooms(user_id))
			.await;
	}

	Ok(())
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
async fn clear_profile(&self, user_id: &UserId) {
	self.services
		.profile
		.clear_profile_keys(user_id)
		.inspect_err(|error| {
			warn!(%user_id, %error, "Failed to clear the profile during deactivation");
		})
		.ok()
		.await;
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
async fn demote_joined_rooms(&self, user_id: &UserId) -> Result {
	self.services
		.state_cache
		.rooms_joined(user_id)
		.map(ToOwned::to_owned)
		.then(async |room_id| self.demote_room(user_id, &room_id).await)
		.try_collect()
		.await
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
async fn demote_room(&self, user_id: &UserId, room_id: &RoomId) -> Result {
	let state_lock = self.services.state.mutex.lock(room_id).await;
	let power_levels = self
		.services
		.state_accessor
		.get_power_levels(room_id)
		.ok()
		.await;

	let can_change_self = power_levels.as_ref().is_some_and(|power_levels| {
		power_levels.user_can_change_user_power_level(user_id, user_id)
	});

	let can_demote_self = can_change_self
		|| self
			.services
			.state_accessor
			.room_state_get(room_id, &StateEventType::RoomCreate, "")
			.await
			.is_ok_and(|event| event.sender() == user_id);

	if !can_demote_self {
		return Ok(());
	}

	let power_levels: RoomPowerLevelsEventContent = power_levels
		.map(TryInto::try_into)
		.transpose()?
		.unwrap_or_default();

	let power_levels = without_user(power_levels, user_id);

	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &power_levels),
			user_id,
			room_id,
			&state_lock,
		)
		.inspect_err(|error| {
			warn!(%room_id, %user_id, %error, "Failed to demote user's own power level");
		})
		.inspect_ok(|_| {
			info!(%user_id, %room_id, "Demoted user as part of account deactivation");
		})
		.ok()
		.await;

	Ok(())
}

fn without_user(
	mut power_levels: RoomPowerLevelsEventContent,
	user_id: &UserId,
) -> RoomPowerLevelsEventContent {
	power_levels.users.remove(user_id);
	power_levels
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
fn membership_rooms<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = OwnedRoomId> + Send + 'a {
	self.services
		.state_cache
		.user_memberships(user_id, Some(CURRENT_MEMBERSHIPS))
		.map(|(_, room_id)| room_id.to_owned())
}

#[implement(Service)]
#[tracing::instrument(skip(self, rooms), level = "debug")]
async fn erase_user_data(&self, user_id: &UserId, rooms: &[OwnedRoomId]) {
	self.services.users.set_erased(user_id);

	join(
		self.erase_threepids(user_id),
		self.services
			.account_data
			.erase_user(user_id, None),
	)
	.await;

	self.erase_account_data(user_id, rooms).await;
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
async fn erase_threepids(&self, user_id: &UserId) {
	self.services
		.threepid
		.get_bindings(user_id)
		.map(|binding| binding.address)
		.broad_then(async |address| {
			self.services
				.threepid
				.del_binding(user_id, &address)
				.await;
		})
		.count()
		.await;
}

#[implement(Service)]
#[tracing::instrument(skip(self, rooms), level = "trace")]
async fn erase_account_data(&self, user_id: &UserId, rooms: &[OwnedRoomId]) {
	rooms
		.iter()
		.cloned()
		.stream()
		.chain(
			self.services
				.state_cache
				.rooms_left(user_id)
				.map(ToOwned::to_owned),
		)
		.broad_then(async |room_id| {
			self.services
				.account_data
				.erase_user(user_id, Some(&room_id))
				.await;
		})
		.count()
		.await;
}

#[implement(Service)]
#[tracing::instrument(skip(self, rooms), level = "trace")]
async fn leave_rooms(&self, user_id: &UserId, rooms: impl Stream<Item = OwnedRoomId> + Send) {
	rooms
		.broadn_then(LEAVE_CONCURRENCY, async |room_id| {
			self.leave_room(user_id, &room_id).await;
		})
		.count()
		.await;
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
async fn leave_room(&self, user_id: &UserId, room_id: &RoomId) {
	let state_lock = self.services.state.mutex.lock(room_id).await;

	self.services
		.membership
		.leave(user_id, room_id, None, false, &state_lock)
		.inspect_err(|error| {
			warn!(%user_id, %room_id, %error, "Failed to leave room remotely");
		})
		.ok()
		.await;

	drop(state_lock);
	self.services.state_cache.forget(room_id, user_id);
}
