use std::collections::HashSet;

use futures::StreamExt;
use ruma::{
	OwnedServerName, RoomId, UserId,
	events::{
		AnyStrippedStateEvent, AnySyncStateEvent, GlobalAccountDataEventType,
		RoomAccountDataEventType, StateEventType,
		direct::DirectEvent,
		room::{
			create::RoomCreateEventContent,
			member::{MembershipState, RoomMemberEventContent},
		},
	},
	serde::Raw,
};
use tuwunel_core::{Result, implement, is_not_empty, matrix::PduCount, utils::ReadyExt, warn};
use tuwunel_database::{Json, serialize_key};

/// Update current membership data.
#[implement(super::Service)]
#[tracing::instrument(
		level = "debug",
		skip_all,
		fields(
			%room_id,
			%user_id,
			%sender,
			%count,
			?membership_event,
		),
	)]
#[expect(clippy::too_many_arguments)]
pub async fn update_membership(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
	membership_event: RoomMemberEventContent,
	sender: &UserId,
	last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
	invite_via: Option<Vec<OwnedServerName>>,
	update_joined_count: bool,
	count: PduCount,
) -> Result {
	let membership = membership_event.membership;

	// Keep track what remote users exist by adding them as "deactivated" users
	//
	// TODO: use futures to update remote profiles without blocking the membership
	// update
	#[expect(clippy::collapsible_if)]
	if !self.services.globals.user_is_local(user_id) {
		if !self.services.users.exists(user_id).await {
			self.services
				.users
				.create(user_id, None, None)
				.await?;
		}
	}

	match &membership {
		| MembershipState::Join => {
			// Check if the user never joined this room
			if !self.once_joined(user_id, room_id).await {
				// Add the user ID to the join list then
				self.mark_as_once_joined(user_id, room_id);

				// Check if the room has a predecessor
				if let Ok(Some(predecessor)) = self
					.services
					.state_accessor
					.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
					.await
					.map(|content: RoomCreateEventContent| content.predecessor)
				{
					// Copy old tags to new room
					if let Ok(tag_event) = self
						.services
						.account_data
						.get_room(&predecessor.room_id, user_id, RoomAccountDataEventType::Tag)
						.await
					{
						self.services
							.account_data
							.update(
								Some(room_id),
								user_id,
								RoomAccountDataEventType::Tag,
								&tag_event,
							)
							.await
							.ok();
					}

					// Copy direct chat flag
					if let Ok(mut direct_event) = self
						.services
						.account_data
						.get_global::<DirectEvent>(user_id, GlobalAccountDataEventType::Direct)
						.await
					{
						let mut room_ids_updated = false;
						for room_ids in direct_event.content.0.values_mut() {
							if room_ids.iter().any(|r| r == &predecessor.room_id) {
								room_ids.push(room_id.to_owned());
								room_ids_updated = true;
							}
						}

						if room_ids_updated {
							self.services
								.account_data
								.update(
									None,
									user_id,
									GlobalAccountDataEventType::Direct
										.to_string()
										.into(),
									&serde_json::to_value(&direct_event)
										.expect("to json always works"),
								)
								.await?;
						}
					}
				}
			}

			self.mark_as_joined(user_id, room_id, count);
		},
		| MembershipState::Invite => {
			// We want to know if the sender is ignored by the receiver
			if self
				.services
				.users
				.user_is_ignored(sender, user_id)
				.await
			{
				return Ok(());
			}

			self.mark_as_invited(user_id, room_id, count, last_state, invite_via)
				.await;
		},
		| MembershipState::Leave | MembershipState::Ban => {
			self.mark_as_left(user_id, room_id, count);

			if self.services.globals.user_is_local(user_id)
				&& (self.services.config.forget_forced_upon_leave
					|| self.services.metadata.is_banned(room_id).await
					|| self.services.metadata.is_disabled(room_id).await)
			{
				self.forget(room_id, user_id);
			}
		},
		| _ => {},
	}

	if update_joined_count {
		self.update_joined_count(room_id).await;
	}

	Ok(())
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn update_joined_count(&self, room_id: &RoomId) {
	let mut joinedcount = 0_u64;
	let mut invitedcount = 0_u64;
	let mut knockedcount = 0_u64;
	let mut joined_servers = HashSet::new();

	self.room_members(room_id)
		.ready_for_each(|joined| {
			joined_servers.insert(joined.server_name().to_owned());
			joinedcount = joinedcount.saturating_add(1);
		})
		.await;

	invitedcount = invitedcount.saturating_add(
		self.room_members_invited(room_id)
			.count()
			.await
			.try_into()
			.unwrap_or(0),
	);

	knockedcount = knockedcount.saturating_add(
		self.room_members_knocked(room_id)
			.count()
			.await
			.try_into()
			.unwrap_or(0),
	);

	self.db
		.roomid_joinedcount
		.raw_put(room_id, joinedcount);
	self.db
		.roomid_invitedcount
		.raw_put(room_id, invitedcount);
	self.db
		.roomid_knockedcount
		.raw_put(room_id, knockedcount);

	self.room_servers(room_id)
		.ready_for_each(|old_joined_server| {
			if joined_servers.remove(old_joined_server) {
				return;
			}

			// Server not in room anymore
			let roomserver_id = (room_id, old_joined_server);
			let serverroom_id = (old_joined_server, room_id);

			self.db.roomserverids.del(roomserver_id);
			self.db.serverroomids.del(serverroom_id);
		})
		.await;

	// Now only new servers are in joined_servers anymore
	for server in &joined_servers {
		let roomserver_id = (room_id, server);
		let serverroom_id = (server, room_id);

		self.db.roomserverids.put_raw(roomserver_id, []);
		self.db.serverroomids.put_raw(serverroom_id, []);
	}

	self.appservice_in_room_cache
		.write()
		.expect("locked")
		.remove(room_id);
}

/// Direct DB function to directly mark a user as joined. It is not
/// recommended to use this directly. You most likely should use
/// `update_membership` instead
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub(crate) fn mark_as_joined(&self, user_id: &UserId, room_id: &RoomId, count: PduCount) {
	let userroom_id = (user_id, room_id);
	let userroom_id = serialize_key(userroom_id).expect("failed to serialize userroom_id");

	let roomuser_id = (room_id, user_id);
	let roomuser_id = serialize_key(roomuser_id).expect("failed to serialize roomuser_id");

	self.db
		.userroomid_joinedcount
		.raw_aput::<8, _, _>(&userroom_id, count.into_unsigned());
	self.db
		.roomuserid_joinedcount
		.raw_aput::<8, _, _>(&roomuser_id, count.into_unsigned());

	self.db
		.userroomid_invitestate
		.remove(&userroom_id);
	self.db
		.roomuserid_invitecount
		.remove(&roomuser_id);

	self.db.userroomid_leftstate.remove(&userroom_id);
	self.db.roomuserid_leftcount.remove(&roomuser_id);

	self.db
		.userroomid_knockedstate
		.remove(&userroom_id);
	self.db
		.roomuserid_knockedcount
		.remove(&roomuser_id);

	self.db.roomid_inviteviaservers.remove(room_id);
}

/// Direct DB function to directly mark a user as left. It is not
/// recommended to use this directly. You most likely should use
/// `update_membership` instead
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub(crate) fn mark_as_left(&self, user_id: &UserId, room_id: &RoomId, count: PduCount) {
	let userroom_id = (user_id, room_id);
	let userroom_id = serialize_key(userroom_id).expect("failed to serialize userroom_id");

	let roomuser_id = (room_id, user_id);
	let roomuser_id = serialize_key(roomuser_id).expect("failed to serialize roomuser_id");

	// (timo) TODO
	let leftstate = Vec::<Raw<AnySyncStateEvent>>::new();

	self.db
		.userroomid_leftstate
		.raw_put(&userroom_id, Json(leftstate));
	self.db
		.roomuserid_leftcount
		.raw_aput::<8, _, _>(&roomuser_id, count.into_unsigned());

	self.db
		.userroomid_joinedcount
		.remove(&userroom_id);
	self.db
		.roomuserid_joinedcount
		.remove(&roomuser_id);

	self.db
		.userroomid_invitestate
		.remove(&userroom_id);
	self.db
		.roomuserid_invitecount
		.remove(&roomuser_id);

	self.db
		.userroomid_knockedstate
		.remove(&userroom_id);
	self.db
		.roomuserid_knockedcount
		.remove(&roomuser_id);

	self.db.roomid_inviteviaservers.remove(room_id);
}

/// Direct DB function to directly mark a user as knocked. It is not
/// recommended to use this directly. You most likely should use
/// `update_membership` instead
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub(crate) fn _mark_as_knocked(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	count: PduCount,
	knocked_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
) {
	let userroom_id = (user_id, room_id);
	let userroom_id = serialize_key(userroom_id).expect("failed to serialize userroom_id");

	let roomuser_id = (room_id, user_id);
	let roomuser_id = serialize_key(roomuser_id).expect("failed to serialize roomuser_id");

	self.db
		.userroomid_knockedstate
		.raw_put(&userroom_id, Json(knocked_state.unwrap_or_default()));
	self.db
		.roomuserid_knockedcount
		.raw_aput::<8, _, _>(&roomuser_id, count.into_unsigned());

	self.db
		.userroomid_joinedcount
		.remove(&userroom_id);
	self.db
		.roomuserid_joinedcount
		.remove(&roomuser_id);

	self.db
		.userroomid_invitestate
		.remove(&userroom_id);
	self.db
		.roomuserid_invitecount
		.remove(&roomuser_id);

	self.db.userroomid_leftstate.remove(&userroom_id);
	self.db.roomuserid_leftcount.remove(&roomuser_id);

	self.db.roomid_inviteviaservers.remove(room_id);
}

/// Makes a user forget a room.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn forget(&self, room_id: &RoomId, user_id: &UserId) {
	let userroom_id = (user_id, room_id);
	let roomuser_id = (room_id, user_id);

	self.db.userroomid_leftstate.del(userroom_id);
	self.db.roomuserid_leftcount.del(roomuser_id);
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
fn mark_as_once_joined(&self, user_id: &UserId, room_id: &RoomId) {
	let key = (user_id, room_id);
	self.db.roomuseroncejoinedids.put_raw(key, []);
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, last_state, invite_via))]
pub(crate) async fn mark_as_invited(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	count: PduCount,
	last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
	invite_via: Option<Vec<OwnedServerName>>,
) {
	let roomuser_id = (room_id, user_id);
	let roomuser_id = serialize_key(roomuser_id).expect("failed to serialize roomuser_id");

	let userroom_id = (user_id, room_id);
	let userroom_id = serialize_key(userroom_id).expect("failed to serialize userroom_id");

	self.db
		.userroomid_invitestate
		.raw_put(&userroom_id, Json(last_state.unwrap_or_default()));
	self.db
		.roomuserid_invitecount
		.raw_aput::<8, _, _>(&roomuser_id, count.into_unsigned());

	self.db
		.userroomid_joinedcount
		.remove(&userroom_id);
	self.db
		.roomuserid_joinedcount
		.remove(&roomuser_id);

	self.db.userroomid_leftstate.remove(&userroom_id);
	self.db.roomuserid_leftcount.remove(&roomuser_id);

	self.db
		.userroomid_knockedstate
		.remove(&userroom_id);
	self.db
		.roomuserid_knockedcount
		.remove(&roomuser_id);

	if let Some(servers) = invite_via.filter(is_not_empty!()) {
		self.add_servers_invite_via(room_id, servers)
			.await;
	}
}
