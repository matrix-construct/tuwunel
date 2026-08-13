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
use tuwunel_core::{
	Result, implement, is_not_empty,
	matrix::PduCount,
	utils::{ReadyExt, result::LogErr},
	warn,
};
use tuwunel_database::{Json, serialize_key, serialize_val};

/// Optional stripped room state attached to invite and knock transitions.
pub type StrippedRoomState = Option<Vec<Raw<AnyStrippedStateEvent>>>;

/// Parameters for one membership cache transition.
///
/// Borrowed identifiers remain valid only for the duration of the update. Owned
/// event data is consumed by the selected transition.
pub struct MembershipUpdate<'a> {
	/// Room whose membership changed.
	///
	/// Membership indexes and aggregate counts are updated for this room.
	pub room_id: &'a RoomId,

	/// User whose membership changed.
	///
	/// Both local and remote users are represented in the membership indexes.
	pub user_id: &'a UserId,

	/// Membership event content driving the transition.
	///
	/// The membership state selects which indexes are written and cleared.
	pub membership_event: RoomMemberEventContent,

	/// User who sent the membership event.
	///
	/// Invite handling uses the sender when applying the ignored-user policy.
	pub sender: &'a UserId,

	/// Stripped room state associated with an invite or knock.
	///
	/// Other membership transitions leave this value unused.
	pub last_state: StrippedRoomState,

	/// Servers supplied as routing hints for an invite.
	///
	/// Invite handling stores only non-empty lists. The routing hints commit
	/// with the membership indexes.
	pub invite_via: Option<Vec<OwnedServerName>>,

	/// Whether to rebuild the room's aggregate membership counts.
	///
	/// Bulk state updates can defer this rebuild until all transitions are
	/// applied.
	pub update_joined_count: bool,

	/// Stream position associated with the membership event.
	///
	/// Count-indexed membership rows store its unsigned representation.
	pub count: PduCount,
}

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
pub async fn update_membership(
	&self,
	MembershipUpdate {
		room_id,
		user_id,
		membership_event,
		sender,
		last_state,
		invite_via,
		update_joined_count,
		count,
	}: MembershipUpdate<'_>,
) -> Result {
	let membership = membership_event.membership;

	self.ensure_remote_user(user_id).await?;

	match membership {
		| MembershipState::Join => {
			self.handle_join(room_id, user_id, count).await?;
		},
		| MembershipState::Invite => {
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
			self.handle_leave(room_id, user_id, count).await;

			// A departure drops the room from the account-wide badge total.
			if self.services.globals.user_is_local(user_id) {
				self.services
					.sending
					.refresh_push_badge(user_id)
					.await
					.log_err()
					.ok();
			}
		},
		| MembershipState::Knock => {
			self.mark_as_knocked(user_id, room_id, count, last_state);
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

	let joinedcount = joinedcount.to_be_bytes();
	let invitedcount = invitedcount.to_be_bytes();
	let knockedcount = knockedcount.to_be_bytes();
	let mut txn = self.services.db.txn();

	txn.insert_raw(&self.db.roomid_joinedcount, room_id, joinedcount);
	txn.insert_raw(&self.db.roomid_invitedcount, room_id, invitedcount);
	txn.insert_raw(&self.db.roomid_knockedcount, room_id, knockedcount);

	self.room_servers(room_id)
		.ready_for_each(|old_joined_server| {
			if joined_servers.remove(old_joined_server) {
				return;
			}

			// Server not in room anymore
			let roomserver_id = (room_id, old_joined_server);
			let serverroom_id = (old_joined_server, room_id);

			txn.del(&self.db.roomserverids, roomserver_id);
			txn.del(&self.db.serverroomids, serverroom_id);
		})
		.await;

	// Now only new servers are in joined_servers anymore
	for server in &joined_servers {
		let roomserver_id = (room_id, server);
		let serverroom_id = (server, room_id);
		let roomserver_id =
			serialize_key(roomserver_id).expect("failed to serialize roomserver_id");

		let serverroom_id =
			serialize_key(serverroom_id).expect("failed to serialize serverroom_id");

		txn.insert_raw(&self.db.roomserverids, roomserver_id, []);
		txn.insert_raw(&self.db.serverroomids, serverroom_id, []);
	}

	txn.execute();

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

	let count = count.into_unsigned().to_be_bytes();
	let mut txn = self.services.db.txn();

	txn.insert_raw(&self.db.userroomid_joinedcount, &userroom_id, count);
	txn.insert_raw(&self.db.roomuserid_joinedcount, &roomuser_id, count);
	txn.del_raw(&self.db.userroomid_invitestate, &userroom_id);
	txn.del_raw(&self.db.roomuserid_invitecount, &roomuser_id);
	txn.del_raw(&self.db.userroomid_leftstate, &userroom_id);
	txn.del_raw(&self.db.roomuserid_leftcount, &roomuser_id);
	txn.del_raw(&self.db.userroomid_knockedstate, &userroom_id);
	txn.del_raw(&self.db.roomuserid_knockedcount, &roomuser_id);
	txn.execute();
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

	let leftstate = serialize_val(Json(Vec::<Raw<AnySyncStateEvent>>::new()))
		.expect("failed to serialize left state");

	let count = count.into_unsigned().to_be_bytes();
	let mut txn = self.services.db.txn();

	txn.insert_raw(&self.db.userroomid_leftstate, &userroom_id, leftstate);
	txn.insert_raw(&self.db.roomuserid_leftcount, &roomuser_id, count);
	txn.del_raw(&self.db.userroomid_joinedcount, &userroom_id);
	txn.del_raw(&self.db.roomuserid_joinedcount, &roomuser_id);
	txn.del_raw(&self.db.userroomid_invitestate, &userroom_id);
	txn.del_raw(&self.db.roomuserid_invitecount, &roomuser_id);
	txn.del_raw(&self.db.userroomid_knockedstate, &userroom_id);
	txn.del_raw(&self.db.roomuserid_knockedcount, &roomuser_id);
	txn.execute();
}

/// Direct DB function to directly mark a user as knocked. It is not
/// recommended to use this directly. You most likely should use
/// `update_membership` instead
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub(crate) fn mark_as_knocked(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	count: PduCount,
	knocked_state: StrippedRoomState,
) {
	let userroom_id = (user_id, room_id);
	let userroom_id = serialize_key(userroom_id).expect("failed to serialize userroom_id");

	let roomuser_id = (room_id, user_id);
	let roomuser_id = serialize_key(roomuser_id).expect("failed to serialize roomuser_id");

	let knocked_state = serialize_val(Json(knocked_state.unwrap_or_default()))
		.expect("failed to serialize knocked state");

	let count = count.into_unsigned().to_be_bytes();
	let mut txn = self.services.db.txn();

	txn.insert_raw(&self.db.userroomid_knockedstate, &userroom_id, knocked_state);
	txn.insert_raw(&self.db.roomuserid_knockedcount, &roomuser_id, count);
	txn.del_raw(&self.db.userroomid_joinedcount, &userroom_id);
	txn.del_raw(&self.db.roomuserid_joinedcount, &roomuser_id);
	txn.del_raw(&self.db.userroomid_invitestate, &userroom_id);
	txn.del_raw(&self.db.roomuserid_invitecount, &roomuser_id);
	txn.del_raw(&self.db.userroomid_leftstate, &userroom_id);
	txn.del_raw(&self.db.roomuserid_leftcount, &roomuser_id);
	txn.execute();
}

/// Makes a user forget a room.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn forget(&self, room_id: &RoomId, user_id: &UserId) {
	let userroom_id = (user_id, room_id);
	let roomuser_id = (room_id, user_id);
	let mut txn = self.services.db.txn();

	txn.del(&self.db.userroomid_leftstate, userroom_id);
	txn.del(&self.db.roomuserid_leftcount, roomuser_id);
	txn.execute();
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
fn mark_as_once_joined(&self, user_id: &UserId, room_id: &RoomId) {
	let key = (user_id, room_id);
	let key = serialize_key(key).expect("failed to serialize roomuseroncejoinedid");
	let mut txn = self.services.db.txn();

	txn.insert_raw(&self.db.roomuseroncejoinedids, key, []);
	txn.execute();
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, last_state, invite_via))]
pub(crate) async fn mark_as_invited(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	count: PduCount,
	last_state: StrippedRoomState,
	invite_via: Option<Vec<OwnedServerName>>,
) {
	let userroom_id = (user_id, room_id);
	let userroom_id = serialize_key(userroom_id).expect("failed to serialize userroom_id");

	let roomuser_id = (room_id, user_id);
	let roomuser_id = serialize_key(roomuser_id).expect("failed to serialize roomuser_id");

	let invite_state = serialize_val(Json(last_state.unwrap_or_default()))
		.expect("failed to serialize invite state");

	let count = count.into_unsigned().to_be_bytes();
	let mut txn = self.services.db.txn();

	txn.insert_raw(&self.db.userroomid_invitestate, &userroom_id, invite_state);
	txn.insert_raw(&self.db.roomuserid_invitecount, &roomuser_id, count);
	txn.del_raw(&self.db.userroomid_joinedcount, &userroom_id);
	txn.del_raw(&self.db.roomuserid_joinedcount, &roomuser_id);
	txn.del_raw(&self.db.userroomid_leftstate, &userroom_id);
	txn.del_raw(&self.db.roomuserid_leftcount, &roomuser_id);
	txn.del_raw(&self.db.userroomid_knockedstate, &userroom_id);
	txn.del_raw(&self.db.roomuserid_knockedcount, &roomuser_id);

	if let Some(servers) = invite_via.filter(is_not_empty!()) {
		self.add_servers_invite_via(&mut txn, room_id, servers)
			.await;
	}

	txn.execute();
}

#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
async fn ensure_remote_user(&self, user_id: &UserId) -> Result {
	if self.services.globals.user_is_local(user_id) || self.services.users.exists(user_id).await {
		return Ok(());
	}

	self.services
		.users
		.create(user_id, None, None)
		.await
}

#[implement(super::Service)]
async fn handle_join(&self, room_id: &RoomId, user_id: &UserId, count: PduCount) -> Result {
	if !self.once_joined(user_id, room_id).await {
		self.mark_as_once_joined(user_id, room_id);
		self.copy_predecessor_data(room_id, user_id)
			.await?;
	}

	self.mark_as_joined(user_id, room_id, count);

	Ok(())
}

#[implement(super::Service)]
async fn copy_predecessor_data(&self, room_id: &RoomId, user_id: &UserId) -> Result {
	let predecessor = self
		.services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
		.await
		.map(|content: RoomCreateEventContent| content.predecessor);

	let Ok(Some(predecessor)) = predecessor else {
		return Ok(());
	};

	self.copy_predecessor_tags(room_id, user_id, &predecessor.room_id)
		.await;

	self.copy_predecessor_direct(room_id, user_id, &predecessor.room_id)
		.await
}

#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
async fn copy_predecessor_tags(&self, room_id: &RoomId, user_id: &UserId, predecessor: &RoomId) {
	let Ok(tag_event) = self
		.services
		.account_data
		.get_room(predecessor, user_id, RoomAccountDataEventType::Tag)
		.await
	else {
		return;
	};

	self.services
		.account_data
		.update(Some(room_id), user_id, RoomAccountDataEventType::Tag, &tag_event)
		.await
		.ok();
}

#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
async fn copy_predecessor_direct(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
	predecessor: &RoomId,
) -> Result {
	let Ok(mut direct_event) = self
		.services
		.account_data
		.get_global::<DirectEvent>(user_id, GlobalAccountDataEventType::Direct)
		.await
	else {
		return Ok(());
	};

	let room_ids_updated =
		direct_event
			.content
			.0
			.values_mut()
			.fold(false, |updated, room_ids| {
				if !room_ids
					.iter()
					.any(|direct_room_id| direct_room_id == predecessor)
				{
					return updated;
				}

				room_ids.push(room_id.to_owned());

				true
			});

	if !room_ids_updated {
		return Ok(());
	}

	let event_type = GlobalAccountDataEventType::Direct
		.to_string()
		.into();

	let direct_event = serde_json::to_value(&direct_event).expect("to json always works");

	self.services
		.account_data
		.update(None, user_id, event_type, &direct_event)
		.await
}

#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
async fn handle_leave(&self, room_id: &RoomId, user_id: &UserId, count: PduCount) {
	self.mark_as_left(user_id, room_id, count);

	if self.services.globals.user_is_local(user_id)
		&& (self.services.config.forget_forced_upon_leave
			|| self.services.metadata.is_banned(room_id).await
			|| self.services.metadata.is_disabled(room_id).await)
	{
		self.forget(room_id, user_id);
	}
}
