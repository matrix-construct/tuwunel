mod update;
mod via;

use std::{
	collections::HashMap,
	sync::{Arc, RwLock},
};

use futures::{Stream, StreamExt, future::join5, pin_mut};
use ruma::{
	OwnedRoomId, RoomId, ServerName, UserId,
	events::{AnyStrippedStateEvent, AnySyncStateEvent, room::member::MembershipState},
	serde::Raw,
};
use tuwunel_core::{
	Result, implement,
	result::LogErr,
	trace,
	utils::{
		BoolExt,
		future::OptionStream,
		stream::{BroadbandExt, ReadyExt, TryIgnore},
	},
	warn,
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Map};

use crate::appservice::RegistrationInfo;

pub struct Service {
	appservice_in_room_cache: AppServiceInRoomCache,
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	roomid_knockedcount: Arc<Map>,
	roomid_invitedcount: Arc<Map>,
	roomid_inviteviaservers: Arc<Map>,
	roomid_joinedcount: Arc<Map>,
	roomserverids: Arc<Map>,
	roomuserid_invitecount: Arc<Map>,
	roomuserid_joinedcount: Arc<Map>,
	roomuserid_leftcount: Arc<Map>,
	roomuserid_knockedcount: Arc<Map>,
	roomuseroncejoinedids: Arc<Map>,
	serverroomids: Arc<Map>,
	userroomid_invitestate: Arc<Map>,
	userroomid_joinedcount: Arc<Map>,
	userroomid_leftstate: Arc<Map>,
	userroomid_knockedstate: Arc<Map>,
}

type AppServiceInRoomCache = RwLock<HashMap<OwnedRoomId, HashMap<String, bool>>>;
type StrippedStateEventItem = (OwnedRoomId, Vec<Raw<AnyStrippedStateEvent>>);
type SyncStateEventItem = (OwnedRoomId, Vec<Raw<AnySyncStateEvent>>);

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			appservice_in_room_cache: RwLock::new(HashMap::new()),
			services: args.services.clone(),
			db: Data {
				roomid_knockedcount: args.db["roomid_knockedcount"].clone(),
				roomid_invitedcount: args.db["roomid_invitedcount"].clone(),
				roomid_inviteviaservers: args.db["roomid_inviteviaservers"].clone(),
				roomid_joinedcount: args.db["roomid_joinedcount"].clone(),
				roomserverids: args.db["roomserverids"].clone(),
				roomuserid_invitecount: args.db["roomuserid_invitecount"].clone(),
				roomuserid_joinedcount: args.db["roomuserid_joined"].clone(),
				roomuserid_leftcount: args.db["roomuserid_leftcount"].clone(),
				roomuserid_knockedcount: args.db["roomuserid_knockedcount"].clone(),
				roomuseroncejoinedids: args.db["roomuseroncejoinedids"].clone(),
				serverroomids: args.db["serverroomids"].clone(),
				userroomid_invitestate: args.db["userroomid_invitestate"].clone(),
				userroomid_joinedcount: args.db["userroomid_joined"].clone(),
				userroomid_leftstate: args.db["userroomid_leftstate"].clone(),
				userroomid_knockedstate: args.db["userroomid_knockedstate"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub async fn appservice_in_room(&self, room_id: &RoomId, appservice: &RegistrationInfo) -> bool {
	if let Some(cached) = self
		.appservice_in_room_cache
		.read()
		.expect("locked")
		.get(room_id)
		.and_then(|map| map.get(&appservice.registration.id))
		.copied()
	{
		return cached;
	}

	let bridge_user_id = UserId::parse_with_server_name(
		appservice.registration.sender_localpart.as_str(),
		self.services.globals.server_name(),
	);

	let Ok(bridge_user_id) = bridge_user_id.log_err() else {
		return false;
	};

	let in_room = self.is_joined(&bridge_user_id, room_id).await
		|| self
			.room_members(room_id)
			.ready_any(|user_id| appservice.users.is_match(user_id.as_str()))
			.await;

	self.appservice_in_room_cache
		.write()
		.expect("locked")
		.entry(room_id.into())
		.or_default()
		.insert(appservice.registration.id.clone(), in_room);

	in_room
}

#[implement(Service)]
pub fn get_appservice_in_room_cache_usage(&self) -> (usize, usize) {
	let cache = self
		.appservice_in_room_cache
		.read()
		.expect("locked");

	(cache.len(), cache.capacity())
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub fn clear_appservice_in_room_cache(&self) {
	self.appservice_in_room_cache
		.write()
		.expect("locked")
		.clear();
}

/// Returns an iterator of all servers participating in this room.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_servers<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &ServerName> + Send + 'a {
	let prefix = (room_id, Interfix);
	self.db
		.roomserverids
		.keys_prefix(&prefix)
		.ignore_err()
		.map(|(_, server): (Ignore, &ServerName)| server)
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn server_in_room<'a>(&'a self, server: &'a ServerName, room_id: &'a RoomId) -> bool {
	let key = (server, room_id);
	self.db.serverroomids.qry(&key).await.is_ok()
}

/// Returns an iterator of all rooms a server participates in (as far as we
/// know).
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn server_rooms<'a>(
	&'a self,
	server: &'a ServerName,
) -> impl Stream<Item = &RoomId> + Send + 'a {
	let prefix = (server, Interfix);
	self.db
		.serverroomids
		.keys_prefix(&prefix)
		.ignore_err()
		.map(|(_, room_id): (Ignore, &RoomId)| room_id)
}

/// Returns true if server can see user by sharing at least one room.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn server_sees_user(&self, server: &ServerName, user_id: &UserId) -> bool {
	self.server_rooms(server)
		.map(ToOwned::to_owned)
		.broad_any(async |room_id| self.is_joined(user_id, &room_id).await)
		.await
}

/// Returns true if user_a and user_b share at least one room.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn user_sees_user(&self, user_a: &UserId, user_b: &UserId) -> bool {
	let get_shared_rooms = self.get_shared_rooms(user_a, user_b);

	pin_mut!(get_shared_rooms);
	get_shared_rooms.next().await.is_some()
}

/// List the rooms common between two users
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn get_shared_rooms<'a>(
	&'a self,
	user_a: &'a UserId,
	user_b: &'a UserId,
) -> impl Stream<Item = &RoomId> + Send + 'a {
	use tuwunel_core::utils::set;

	let a = self.rooms_joined(user_a);
	let b = self.rooms_joined(user_b).boxed();

	set::intersection_sorted_stream2(a, b)
}

/// Returns an iterator of all joined members of a room.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_members<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	let prefix = (room_id, Interfix);
	self.db
		.roomuserid_joinedcount
		.keys_prefix(&prefix)
		.ignore_err()
		.map(|(_, user_id): (Ignore, &UserId)| user_id)
}

/// Returns the number of users which are currently in a room
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn room_joined_count(&self, room_id: &RoomId) -> Result<u64> {
	self.db
		.roomid_joinedcount
		.get(room_id)
		.await
		.deserialized()
}

/// Returns the number of users which are currently invited to a room
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn room_invited_count(&self, room_id: &RoomId) -> Result<u64> {
	self.db
		.roomid_invitedcount
		.get(room_id)
		.await
		.deserialized()
}

/// Returns the number of users which are currently knocking upon a room
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn room_knocked_count(&self, room_id: &RoomId) -> Result<u64> {
	self.db
		.roomid_knockedcount
		.get(room_id)
		.await
		.deserialized()
}

/// Returns an iterator of all our local joined users in a room who are
/// active (not deactivated, not guest)
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn active_local_users_in_room<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	self.local_users_in_room(room_id)
		.filter(|user| self.services.users.is_active(user))
}

/// Returns an iterator of all our local users in the room, even if they're
/// deactivated/guests
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn local_users_in_room<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	self.room_members(room_id)
		.ready_filter(|user| self.services.globals.user_is_local(user))
}

/// Returns an iterator of only our users invited to this room.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn local_users_invited_to_room<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	self.room_members_invited(room_id)
		.ready_filter(|user| self.services.globals.user_is_local(user))
}

/// Returns an iterator over all User IDs who ever joined a room.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_useroncejoined<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	let prefix = (room_id, Interfix);
	self.db
		.roomuseroncejoinedids
		.keys_prefix(&prefix)
		.ignore_err()
		.map(|(_, user_id): (Ignore, &UserId)| user_id)
}

/// Returns an iterator over all invited members of a room.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_members_invited<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	let prefix = (room_id, Interfix);
	self.db
		.roomuserid_invitecount
		.keys_prefix(&prefix)
		.ignore_err()
		.map(|(_, user_id): (Ignore, &UserId)| user_id)
}

/// Returns an iterator over all knocked members of a room.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_members_knocked<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	let prefix = (room_id, Interfix);
	self.db
		.roomuserid_knockedcount
		.keys_prefix(&prefix)
		.ignore_err()
		.map(|(_, user_id): (Ignore, &UserId)| user_id)
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn get_invite_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<u64> {
	let key = (room_id, user_id);
	self.db
		.roomuserid_invitecount
		.qry(&key)
		.await
		.deserialized()
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn get_knock_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<u64> {
	let key = (room_id, user_id);
	self.db
		.roomuserid_knockedcount
		.qry(&key)
		.await
		.deserialized()
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn get_left_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<u64> {
	let key = (room_id, user_id);
	self.db
		.roomuserid_leftcount
		.qry(&key)
		.await
		.deserialized()
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn get_joined_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<u64> {
	let key = (room_id, user_id);
	self.db
		.roomuserid_joinedcount
		.qry(&key)
		.await
		.deserialized()
}

/// Returns an iterator over all memberships for a user.
#[implement(Service)]
#[inline]
pub fn all_user_memberships<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = (MembershipState, &RoomId)> + Send + 'a {
	self.user_memberships(user_id, None)
}

/// Returns an iterator over all specified memberships for a user.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn user_memberships<'a>(
	&'a self,
	user_id: &'a UserId,
	mask: Option<&[MembershipState]>,
) -> impl Stream<Item = (MembershipState, &RoomId)> + Send + 'a {
	use MembershipState::*;
	use futures::stream::select;

	let joined = mask
		.is_none_or(|mask| mask.contains(&Join))
		.then_async(|| {
			self.rooms_joined(user_id)
				.map(|room_id| (Join, room_id))
				.boxed()
				.into_future()
		});

	let invited = mask
		.is_none_or(|mask| mask.contains(&Invite))
		.then_async(|| {
			self.rooms_invited(user_id)
				.map(|room_id| (Invite, room_id))
				.boxed()
				.into_future()
		});

	let knocked = mask
		.is_none_or(|mask| mask.contains(&Knock))
		.then_async(|| {
			self.rooms_knocked(user_id)
				.map(|room_id| (Knock, room_id))
				.boxed()
				.into_future()
		});

	let left = mask
		.is_none_or(|mask| mask.contains(&Leave))
		.then_async(|| {
			self.rooms_left(user_id)
				.map(|room_id| (Leave, room_id))
				.boxed()
				.into_future()
		});

	select(
		select(joined.stream(), left.stream()),
		select(invited.stream(), knocked.stream()),
	)
}

/// Returns an iterator over all rooms this user joined.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn rooms_joined<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = &RoomId> + Send + 'a {
	self.db
		.userroomid_joinedcount
		.keys_raw_prefix(user_id)
		.ignore_err()
		.map(|(_, room_id): (Ignore, &RoomId)| room_id)
}

/// Returns an iterator over all rooms a user was invited to.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn rooms_invited<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = &RoomId> + Send + 'a {
	self.db
		.userroomid_invitestate
		.keys_raw_prefix(user_id)
		.ignore_err()
		.map(|(_, room_id): (Ignore, &RoomId)| room_id)
}

/// Returns an iterator over all rooms a user is currently knocking.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn rooms_knocked<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = &RoomId> + Send + 'a {
	self.db
		.userroomid_knockedstate
		.keys_raw_prefix(user_id)
		.ignore_err()
		.map(|(_, room_id): (Ignore, &RoomId)| room_id)
}

/// Returns an iterator over all rooms a user left.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn rooms_left<'a>(&'a self, user_id: &'a UserId) -> impl Stream<Item = &RoomId> + Send + 'a {
	self.db
		.userroomid_leftstate
		.keys_raw_prefix(user_id)
		.ignore_err()
		.map(|(_, room_id): (Ignore, &RoomId)| room_id)
}

/// Returns an iterator over all rooms a user was invited to.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn rooms_invited_state<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = StrippedStateEventItem> + Send + 'a {
	type KeyVal<'a> = (Key<'a>, Raw<Vec<AnyStrippedStateEvent>>);
	type Key<'a> = (&'a UserId, &'a RoomId);

	let prefix = (user_id, Interfix);
	self.db
		.userroomid_invitestate
		.stream_prefix(&prefix)
		.ignore_err()
		.map(|((_, room_id), state): KeyVal<'_>| (room_id.to_owned(), state))
		.map(|(room_id, state)| Ok((room_id, state.deserialize_as_unchecked()?)))
		.ignore_err()
}

/// Returns an iterator over all rooms a user is currently knocking.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub fn rooms_knocked_state<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = StrippedStateEventItem> + Send + 'a {
	type KeyVal<'a> = (Key<'a>, Raw<Vec<AnyStrippedStateEvent>>);
	type Key<'a> = (&'a UserId, &'a RoomId);

	let prefix = (user_id, Interfix);
	self.db
		.userroomid_knockedstate
		.stream_prefix(&prefix)
		.ignore_err()
		.map(|((_, room_id), state): KeyVal<'_>| (room_id.to_owned(), state))
		.map(|(room_id, state)| Ok((room_id, state.deserialize_as_unchecked()?)))
		.ignore_err()
}

/// Returns an iterator over all rooms a user left.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn rooms_left_state<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = SyncStateEventItem> + Send + 'a {
	type KeyVal<'a> = (Key<'a>, Raw<Vec<Raw<AnySyncStateEvent>>>);
	type Key<'a> = (&'a UserId, &'a RoomId);

	let prefix = (user_id, Interfix);
	self.db
		.userroomid_leftstate
		.stream_prefix(&prefix)
		.ignore_err()
		.map(|((_, room_id), state): KeyVal<'_>| (room_id.to_owned(), state))
		.map(|(room_id, state)| Ok((room_id, state.deserialize_as_unchecked()?)))
		.ignore_err()
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn invite_state(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<Vec<Raw<AnyStrippedStateEvent>>> {
	let key = (user_id, room_id);
	self.db
		.userroomid_invitestate
		.qry(&key)
		.await
		.deserialized()
		.and_then(|val: Raw<Vec<AnyStrippedStateEvent>>| {
			val.deserialize_as_unchecked().map_err(Into::into)
		})
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn knock_state(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<Vec<Raw<AnyStrippedStateEvent>>> {
	let key = (user_id, room_id);
	self.db
		.userroomid_knockedstate
		.qry(&key)
		.await
		.deserialized()
		.and_then(|val: Raw<Vec<AnyStrippedStateEvent>>| {
			val.deserialize_as_unchecked().map_err(Into::into)
		})
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn left_state(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<Vec<Raw<AnyStrippedStateEvent>>> {
	let key = (user_id, room_id);
	self.db
		.userroomid_leftstate
		.qry(&key)
		.await
		.deserialized()
		.and_then(|val: Raw<Vec<AnyStrippedStateEvent>>| {
			val.deserialize_as_unchecked().map_err(Into::into)
		})
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn user_membership(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> Option<MembershipState> {
	let states = join5(
		self.is_joined(user_id, room_id),
		self.is_left(user_id, room_id),
		self.is_knocked(user_id, room_id),
		self.is_invited(user_id, room_id),
		self.once_joined(user_id, room_id),
	)
	.await;

	match states {
		| (true, ..) => Some(MembershipState::Join),
		| (_, true, ..) => Some(MembershipState::Leave),
		| (_, _, true, ..) => Some(MembershipState::Knock),
		| (_, _, _, true, ..) => Some(MembershipState::Invite),
		| (false, false, false, false, true) => Some(MembershipState::Ban),
		| _ => None,
	}
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	let key = (user_id, room_id);
	self.db.roomuseroncejoinedids.contains(&key).await
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn is_joined<'a>(&'a self, user_id: &'a UserId, room_id: &'a RoomId) -> bool {
	let key = (user_id, room_id);
	self.db
		.userroomid_joinedcount
		.contains(&key)
		.await
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn is_knocked<'a>(&'a self, user_id: &'a UserId, room_id: &'a RoomId) -> bool {
	let key = (user_id, room_id);
	self.db
		.userroomid_knockedstate
		.contains(&key)
		.await
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	let key = (user_id, room_id);
	self.db
		.userroomid_invitestate
		.contains(&key)
		.await
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	let key = (user_id, room_id);
	self.db.userroomid_leftstate.contains(&key).await
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn delete_room_join_counts(&self, room_id: &RoomId, force: bool) -> Result {
	let prefix = (room_id, Interfix);

	self.db.roomid_knockedcount.remove(room_id);

	self.db.roomid_invitedcount.remove(room_id);

	self.db.roomid_inviteviaservers.remove(room_id);

	self.db.roomid_joinedcount.remove(room_id);

	self.db
		.roomserverids
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key: (&RoomId, &ServerName)| {
			trace!("Removing key: {key:?}");
			self.db.roomserverids.del(key);

			let reverse_key = (key.1, key.0);
			trace!("Removing reverse key: {reverse_key:?}");
			self.db.serverroomids.del(reverse_key);
		})
		.await;

	self.db
		.roomuserid_invitecount
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key: (&RoomId, &UserId)| {
			trace!("Removing key: {key:?}");
			self.db.roomuserid_invitecount.del(key);

			let reverse_key = (key.1, key.0);
			trace!("Removing reverse key: {reverse_key:?}");
			self.db.userroomid_invitestate.del(reverse_key);
		})
		.await;

	self.db
		.roomuserid_joinedcount
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key: (&RoomId, &UserId)| {
			trace!("Removing key: {key:?}");
			self.db.roomuserid_joinedcount.del(key);

			let reverse_key = (key.1, key.0);
			trace!("Removing reverse key: {reverse_key:?}");
			self.db.userroomid_joinedcount.del(reverse_key);
		})
		.await;

	self.db
		.roomuserid_knockedcount
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key: (&RoomId, &UserId)| {
			trace!("Removing key: {key:?}");
			self.db.roomuserid_knockedcount.del(key);

			let reverse_key = (key.1, key.0);
			trace!("Removing reverse key: {reverse_key:?}");
			self.db.userroomid_knockedstate.del(reverse_key);
		})
		.await;

	self.db
		.roomuserid_leftcount
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_filter(|(_, user_id): &(&RoomId, &UserId)| {
			force || !self.services.globals.user_is_local(user_id)
		})
		.ready_for_each(|key: (&RoomId, &UserId)| {
			trace!("Removing key: {key:?}");
			self.db.roomuserid_leftcount.del(key);

			let reverse_key = (key.1, key.0);
			trace!("Removing reverse key: {reverse_key:?}");
			self.db.userroomid_leftstate.del(reverse_key);
		})
		.await;

	Ok(())
}
