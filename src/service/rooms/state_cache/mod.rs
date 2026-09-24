//! Persistent indexes derived from room membership state.
//!
//! The service maintains paired user-to-room and room-to-user membership rows,
//! aggregate counts, participating-server indexes, and cached stripped state.
//! Read helpers expose point queries and cursor-backed streams over those
//! derived indexes.

mod update;
mod via;

use std::{
	collections::HashMap,
	convert::identity,
	sync::{Arc, RwLock},
};

use futures::{
	Stream, StreamExt, TryStreamExt,
	future::join5,
	pin_mut,
	stream::{empty, select},
};
use ruma::{
	OwnedRoomId, OwnedServerName, RoomId, ServerName, UserId,
	events::{AnyStrippedStateEvent, AnySyncStateEvent, room::member::MembershipState},
	serde::Raw,
};
use serde::de::DeserializeOwned;
use tuwunel_core::{
	Result, debug_warn, implement,
	matrix::{Event, Pdu, event::Owned},
	trace,
	utils::{
		self, BoolExt,
		result::NotFound,
		stream::{BroadbandExt, IterStream, ReadyExt, TryIgnore},
	},
	warn,
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Map};
use update::EMPTY_INVITE_STATE;
/// Input types for applying membership-cache transitions.
///
/// The update descriptor owns event data while borrowing the affected user and
/// room identifiers. Optional stripped state accompanies invite and knock
/// transitions.
pub use update::{MembershipUpdate, StrippedRoomState};

use crate::appservice::RegistrationInfo;

/// Persistent room-membership cache and derived-index service.
///
/// Paired indexes answer membership queries in either direction, while room
/// aggregates track counts and participating servers. A process-local cache
/// memoizes appservice membership decisions.
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

/// Tests whether an appservice participates in a room.
///
/// A registration participates when its sender or a namespace-matching user is
/// joined. Results are memoized by room and registration identifier. Membership
/// rebuilds and explicit clears remove cached entries, but do not fence an
/// in-flight lookup from republishing an older result. Deleting a room's
/// membership indexes does not invalidate this cache.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub async fn appservice_in_room(&self, room_id: &RoomId, appservice: &RegistrationInfo) -> bool {
	let cached = self
		.appservice_in_room_cache
		.read()
		.expect("locked")
		.get(room_id)
		.and_then(|map| map.get(&appservice.registration.id))
		.copied();

	if let Some(cached) = cached {
		return cached;
	}

	let in_room = self.is_joined(&appservice.sender, room_id).await
		|| self
			.room_members(room_id)
			.ready_any(|user_id| appservice.is_user_match(user_id))
			.await;

	self.appservice_in_room_cache
		.write()
		.expect("locked")
		.entry(room_id.into())
		.or_default()
		.insert(appservice.registration.id.clone(), in_room);

	in_room
}

/// Returns the appservice membership cache's room count and capacity.
///
/// The first value counts room-level map entries rather than individual
/// registrations. The second reports the backing map's current allocation
/// capacity.
#[implement(Service)]
pub fn get_appservice_in_room_cache_usage(&self) -> (usize, usize) {
	let cache = self
		.appservice_in_room_cache
		.read()
		.expect("locked");

	(cache.len(), cache.capacity())
}

/// Clears every memoized appservice membership decision.
///
/// Persistent membership indexes are not changed. Later uncached lookups
/// recompute and cache their answers from current joined membership, but a
/// lookup already in flight can republish an older result after the clear.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub fn clear_appservice_in_room_cache(&self) {
	self.appservice_in_room_cache
		.write()
		.expect("locked")
		.clear();
}

/// Returns a stream of the remote servers participating in this room.
///
/// Our own server is filtered out, so the result is the federation fan-out
/// set for the room. Items borrow the database cursor and are invalid after the
/// next poll; consume or own each item before advancing. Storage errors are
/// skipped as in [`Self::room_servers`].
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub fn remote_room_servers<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &ServerName> + Send + 'a {
	self.room_servers(room_id)
		.ready_filter(|server| !self.services.globals.server_is_ours(server))
}

/// Streams all servers recorded as participating in a room.
///
/// Storage and key-decoding failures are skipped. Each server name borrows the
/// cursor and must be consumed or owned before the stream advances.
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

/// Tests whether a server is recorded as participating in a room.
///
/// The reverse server-to-room index supplies the answer. Missing rows and
/// storage failures both return `false`.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn server_in_room<'a>(&'a self, server: &'a ServerName, room_id: &'a RoomId) -> bool {
	let key = (server, room_id);
	self.db.serverroomids.qry(&key).await.is_ok()
}

/// Streams all rooms recorded for a participating server.
///
/// Storage and key-decoding failures are skipped. Each room identifier borrows
/// the cursor and must be consumed or owned before the stream advances.
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

/// Streams each server participating in at least one known room.
///
/// Adjacent duplicate prefixes are collapsed, yielding server names in
/// ascending key order. Storage and decoding failures are skipped, and each
/// item borrows the cursor until its next poll.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn servers(&self) -> impl Stream<Item = &ServerName> + Send + '_ {
	self.db
		.serverroomids
		.keys()
		.ignore_err()
		.ready_scan(
			None,
			|last: &mut Option<OwnedServerName>, (server, _): (&ServerName, Ignore)| {
				let fresh = last.as_deref() != Some(server);

				if fresh {
					*last = Some(server.to_owned());
				}

				Some(fresh.then_some(server))
			},
		)
		.ready_filter_map(identity)
}

/// Tests whether a server participates in any known room.
///
/// The check stops at the first reverse-index row. Errors skipped by
/// [`Self::server_rooms`] are indistinguishable from absence.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn server_shares_room(&self, server: &ServerName) -> bool {
	self.server_rooms(server)
		.ready_any(|_| true)
		.await
}

/// Tests whether a server shares a joined room with a user.
///
/// The check searches rooms recorded for the server for any current user
/// join. Index errors are treated as absent rows by the underlying helpers.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn server_sees_user(&self, server: &ServerName, user_id: &UserId) -> bool {
	self.server_rooms(server)
		.map(ToOwned::to_owned)
		.broad_any(async |room_id| self.is_joined(user_id, &room_id).await)
		.await
}

/// Tests whether two users share a currently joined room.
///
/// The check consumes only the first intersection result. Storage or decoding
/// failures skipped by the joined-room streams can produce `false`.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn user_sees_user(&self, user_a: &UserId, user_b: &UserId) -> bool {
	let get_shared_rooms = self.get_shared_rooms(user_a, user_b);

	pin_mut!(get_shared_rooms);
	get_shared_rooms.next().await.is_some()
}

/// Streams rooms in which both users are currently joined.
///
/// The two key-ordered joined-room streams are intersected without
/// materializing either set. Items borrow their source database cursor and are
/// invalid after the next poll; consume or own each item before advancing. Read
/// failures are skipped by the source streams.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn get_shared_rooms<'a>(
	&'a self,
	user_a: &'a UserId,
	user_b: &'a UserId,
) -> impl Stream<Item = &RoomId> + Send + 'a {
	let a = self.rooms_joined(user_a);
	let b = self.rooms_joined(user_b);

	utils::set::intersection_sorted_stream2(a, b)
}

/// Streams all users currently indexed as joined to a room.
///
/// Storage and key-decoding failures are skipped. Each user identifier borrows
/// the cursor and must be consumed or owned before the stream advances.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_members<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	self.room_members_checked(room_id).ignore_err()
}

/// Streams joined users within the room's exact encoded prefix.
///
/// Storage and user-key decoding failures are surfaced as error items rather
/// than dropped. User IDs borrow the cursor and must be consumed or owned
/// before advancing it.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn room_members_checked<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = Result<&'a UserId>> + Send + 'a {
	let prefix = (room_id, Interfix);

	self.db
		.roomuserid_joinedcount
		.keys_prefix(&prefix)
		.map_ok(|(_, user_id): (Ignore, &UserId)| user_id)
}

/// Returns the stored number of users currently joined to a room.
///
/// The aggregate is rebuilt from membership indexes by
/// [`Self::update_joined_count`]. Missing or malformed count rows return an
/// error.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn room_joined_count(&self, room_id: &RoomId) -> Result<u64> {
	self.db
		.roomid_joinedcount
		.get(room_id)
		.await
		.deserialized()
}

/// Returns the stored number of users currently invited to a room.
///
/// The aggregate is rebuilt from membership indexes by
/// [`Self::update_joined_count`]. Missing or malformed count rows return an
/// error.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn room_invited_count(&self, room_id: &RoomId) -> Result<u64> {
	self.db
		.roomid_invitedcount
		.get(room_id)
		.await
		.deserialized()
}

/// Returns the stored number of users currently knocking on a room.
///
/// The aggregate is rebuilt from membership indexes by
/// [`Self::update_joined_count`]. Missing or malformed count rows return an
/// error.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn room_knocked_count(&self, room_id: &RoomId) -> Result<u64> {
	self.db
		.roomid_knockedcount
		.get(room_id)
		.await
		.deserialized()
}

/// Streams active local users currently joined to a room.
///
/// Local joined users are filtered through the user service, excluding guests
/// and deactivated accounts. The stream otherwise inherits
/// [`Self::room_members`]'s cursor lifetime and error policy.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn active_local_users_in_room<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	self.local_users_in_room(room_id)
		.filter(|user| self.services.users.is_active(user))
}

/// Streams all local users currently joined to a room.
///
/// Guest and deactivated accounts remain included. The stream otherwise
/// inherits [`Self::room_members`]'s cursor lifetime and error policy.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn local_users_in_room<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	self.room_members(room_id)
		.ready_filter(|user| self.services.globals.user_is_local(user))
}

/// Streams local users currently invited to a room.
///
/// Remote invitees are filtered out by server name. The stream otherwise
/// inherits [`Self::room_members_invited`]'s cursor lifetime and error policy.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn local_users_invited_to_room<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &UserId> + Send + 'a {
	self.room_members_invited(room_id)
		.ready_filter(|user| self.services.globals.user_is_local(user))
}

/// Streams user identifiers from the once-joined index under a room prefix.
///
/// Once-joined rows are currently written with user-first keys, while this
/// accessor probes a room-first prefix, so the stored layout can yield no
/// matches. Yielded user identifiers borrow the database cursor and are invalid
/// after the next poll; consume or own each item before advancing. Storage and
/// decoding failures are skipped.
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

/// Streams all users currently indexed as invited to a room.
///
/// Storage and key-decoding failures are skipped. Each user identifier borrows
/// the cursor and must be consumed or owned before the stream advances.
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

/// Streams all users currently indexed as knocking on a room.
///
/// Storage and key-decoding failures are skipped. Each user identifier borrows
/// the cursor and must be consumed or owned before the stream advances.
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

/// Returns the stream position associated with a user's current invite.
///
/// This value identifies the membership transition rather than counting
/// invitations. Missing or malformed index rows return an error.
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

/// Returns the stream position associated with a user's current knock.
///
/// This value identifies the membership transition rather than counting
/// knocks. Missing or malformed index rows return an error.
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

/// Returns the stream position associated with a user's current leave row.
///
/// This value identifies the membership transition rather than counting
/// leaves. Missing or malformed index rows return an error.
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

/// Returns the stream position associated with a user's current join.
///
/// This value identifies the membership transition rather than counting
/// joins. A join recorded before positions were stored holds an empty value
/// and reads as zero. Missing or malformed index rows return an error.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn get_joined_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<u64> {
	let key = (room_id, user_id);

	self.db
		.roomuserid_joinedcount
		.qry(&key)
		.await
		.and_then(|value| {
			value
				.is_empty()
				.map_or_else(|| value.deserialized(), || Ok(0))
		})
}

/// Streams every cached membership category for a user.
///
/// Join, leave, invite, and knock indexes are combined without a global
/// ordering guarantee. Yielded room identifiers borrow their source database
/// cursor and are invalid after the next poll; consume or own each item before
/// advancing. Source-stream errors are skipped.
#[implement(Service)]
#[inline]
pub fn all_user_memberships<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = (MembershipState, &RoomId)> + Send + 'a {
	self.user_memberships(user_id, None)
}

/// Streams selected cached membership categories for a user.
///
/// A missing mask selects join, leave, invite, and knock; an empty mask selects
/// none. Category streams are interleaved without a global ordering guarantee,
/// and their storage errors are skipped. Yielded room identifiers borrow their
/// source database cursor and are invalid after the next poll; consume or own
/// each item before advancing.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn user_memberships<'a>(
	&'a self,
	user_id: &'a UserId,
	mask: Option<&[MembershipState]>,
) -> impl Stream<Item = (MembershipState, &RoomId)> + Send + 'a {
	let joined = mask
		.is_none_or(|mask| mask.contains(&MembershipState::Join))
		.then(|| {
			self.rooms_joined(user_id)
				.map(|room_id| (MembershipState::Join, room_id))
				.left_stream()
		})
		.unwrap_or_else(|| empty().right_stream());

	let invited = mask
		.is_none_or(|mask| mask.contains(&MembershipState::Invite))
		.then(|| {
			self.rooms_invited(user_id)
				.map(|room_id| (MembershipState::Invite, room_id))
				.left_stream()
		})
		.unwrap_or_else(|| empty().right_stream());

	let knocked = mask
		.is_none_or(|mask| mask.contains(&MembershipState::Knock))
		.then(|| {
			self.rooms_knocked(user_id)
				.map(|room_id| (MembershipState::Knock, room_id))
				.left_stream()
		})
		.unwrap_or_else(|| empty().right_stream());

	let left = mask
		.is_none_or(|mask| mask.contains(&MembershipState::Leave))
		.then(|| {
			self.rooms_left(user_id)
				.map(|room_id| (MembershipState::Leave, room_id))
				.left_stream()
		})
		.unwrap_or_else(|| empty().right_stream());

	select(select(joined, left), select(invited, knocked))
}

/// Streams rooms in which a user is currently indexed as joined.
///
/// The raw user prefix preserves the established key scan. Storage and
/// decoding failures are skipped, and each room identifier borrows the cursor
/// until its next poll.
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

/// Streams joined rooms within the user's exact encoded prefix.
///
/// Storage and room-key decoding failures are surfaced as error items rather
/// than dropped. Room IDs borrow the cursor and must be consumed or owned
/// before advancing it.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub fn rooms_joined_checked<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = Result<&'a RoomId>> + Send + 'a {
	let prefix = (user_id, Interfix);

	self.db
		.userroomid_joinedcount
		.keys_prefix(&prefix)
		.map_ok(|(_, room_id): (Ignore, &RoomId)| room_id)
}

/// Streams rooms in which a user is currently indexed as invited.
///
/// The raw user prefix preserves the established key scan. Storage and
/// decoding failures are skipped, and each room identifier borrows the cursor
/// until its next poll.
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

/// Streams rooms in which a user is currently indexed as knocking.
///
/// The raw user prefix preserves the established key scan. Storage and
/// decoding failures are skipped, and each room identifier borrows the cursor
/// until its next poll.
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

/// Streams rooms for which a user's leave state is retained.
///
/// Forgotten rooms have no leave row and therefore do not appear. Storage and
/// decoding failures are skipped, and each room identifier borrows the cursor
/// until its next poll.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn rooms_left<'a>(&'a self, user_id: &'a UserId) -> impl Stream<Item = &RoomId> + Send + 'a {
	self.db
		.userroomid_leftstate
		.keys_raw_prefix(user_id)
		.ignore_err()
		.map(|(_, room_id): (Ignore, &RoomId)| room_id)
}

/// Streams stored stripped state for a user's current invitations.
///
/// Each item owns its room identifier and decoded state vector. Storage,
/// key-decoding, and state-deserialization failures are skipped.
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

/// Streams stored stripped state for a user's current knocks.
///
/// Each item owns its room identifier and decoded state vector. Storage,
/// key-decoding, and state-deserialization failures are skipped.
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

/// Streams stored state for rooms a user has left but not forgotten.
///
/// Both native state arrays and compatible single-event rows are accepted.
/// Storage and key-decoding failures are skipped, while unusable state values
/// yield an empty event vector for their room.
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
		.map(|(room_id, state)| {
			let state = state_events(&room_id, &state);

			(room_id, state)
		})
}

/// Returns the stripped state stored for a user's current invitation.
///
/// The value is decoded as an array of stripped state events. Missing rows,
/// storage failures, and malformed state return an error.
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

/// Whether the user's stored invite carries any stripped state.
///
/// The row is probed raw rather than decoded through [`Self::invite_state`], so
/// no stripped state is materialized to answer it. A failed read is an error
/// rather than an absent row.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn has_invite_state(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
	let key = (user_id, room_id);

	self.db
		.userroomid_invitestate
		.qry(&key)
		.await
		.optional()
		.map(|state| state.is_some_and(|state| state.len() > EMPTY_INVITE_STATE.len()))
}

/// Returns the stripped state stored for a user's current knock.
///
/// The value is decoded as an array of stripped state events. Missing rows,
/// storage failures, and malformed state return an error.
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

/// Returns the cached state for a room a user has left.
///
/// Native state arrays and compatible single-event rows are normalized into
/// one vector. Missing or unreadable rows return an error, while an unusable
/// stored JSON shape is logged and becomes an empty vector.
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
		.map(|state: Raw<Vec<AnyStrippedStateEvent>>| state_events(room_id, &state))
}

/// Infers a user's cached membership state for one room.
///
/// Current indexes take precedence in join, leave, knock, then invite order.
/// When none exists, a once-joined marker is reported as `Ban`; no marker
/// returns `None`. Read failures from the boolean probes are treated as
/// absence.
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

/// Tests whether a user has ever been marked as joined to a room.
///
/// The durable marker survives later membership transitions and explicit
/// forget operations. Missing rows and storage failures both return `false`.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	let key = (user_id, room_id);
	self.db.roomuseroncejoinedids.contains(&key).await
}

/// Tests whether a user is currently joined to any of the given rooms.
///
/// The rooms are probed concurrently and the first joined room decides the
/// answer; an empty set of rooms answers `false`. A probe that fails counts
/// as not joined, as it does for [`Self::is_joined`].
#[implement(Service)]
#[tracing::instrument(skip(self, room_ids), level = "trace")]
pub async fn is_joined_any<'a, Rooms>(&self, user_id: &UserId, room_ids: Rooms) -> bool
where
	Rooms: IntoIterator<Item = &'a RoomId> + Send,
	Rooms::IntoIter: Send,
{
	room_ids
		.into_iter()
		.stream()
		.broad_any(|room_id| self.is_joined(user_id, room_id))
		.await
}

/// Tests whether a user is currently indexed as joined to a room.
///
/// The user-to-room join index supplies the answer. Missing rows and storage
/// failures both return `false`.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn is_joined<'a>(&'a self, user_id: &'a UserId, room_id: &'a RoomId) -> bool {
	let key = (user_id, room_id);
	self.db
		.userroomid_joinedcount
		.contains(&key)
		.await
}

/// Tests whether a user is currently indexed as knocking on a room.
///
/// The stored knock-state row supplies the answer. Missing rows and storage
/// failures both return `false`.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn is_knocked<'a>(&'a self, user_id: &'a UserId, room_id: &'a RoomId) -> bool {
	let key = (user_id, room_id);
	self.db
		.userroomid_knockedstate
		.contains(&key)
		.await
}

/// Tests whether a user is currently indexed as invited to a room.
///
/// The stored invite-state row supplies the answer. Missing rows and storage
/// failures both return `false`.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	let key = (user_id, room_id);
	self.db
		.userroomid_invitestate
		.contains(&key)
		.await
}

/// Tests whether a user's leave state is currently retained for a room.
///
/// Explicitly forgotten rooms have no row and return `false`. Missing rows and
/// storage failures are otherwise indistinguishable.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	let key = (user_id, room_id);
	self.db.userroomid_leftstate.contains(&key).await
}

/// Deletes a room's aggregate and paired membership indexes.
///
/// Aggregate rows and every paired index row found during enumeration are
/// deleted in one database transaction. Local users' leave rows survive unless
/// `force` is true, while remote leave rows are always removed. Once-joined
/// markers and appservice membership-cache entries are untouched. Storage
/// errors encountered during enumeration are skipped and can leave index rows
/// behind.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn delete_room_join_counts(&self, room_id: &RoomId, force: bool) -> Result {
	let prefix = (room_id, Interfix);
	let mut txn = self.services.db.txn();

	txn.del_raw(&self.db.roomid_knockedcount, room_id);

	txn.del_raw(&self.db.roomid_invitedcount, room_id);

	txn.del_raw(&self.db.roomid_inviteviaservers, room_id);

	txn.del_raw(&self.db.roomid_joinedcount, room_id);

	self.db
		.roomserverids
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key: (&RoomId, &ServerName)| {
			trace!("Removing key: {key:?}");
			txn.del(&self.db.roomserverids, key);

			let reverse_key = (key.1, key.0);

			trace!("Removing reverse key: {reverse_key:?}");
			txn.del(&self.db.serverroomids, reverse_key);
		})
		.await;

	self.db
		.roomuserid_invitecount
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key: (&RoomId, &UserId)| {
			trace!("Removing key: {key:?}");
			txn.del(&self.db.roomuserid_invitecount, key);

			let reverse_key = (key.1, key.0);

			trace!("Removing reverse key: {reverse_key:?}");
			txn.del(&self.db.userroomid_invitestate, reverse_key);
		})
		.await;

	self.db
		.roomuserid_joinedcount
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key: (&RoomId, &UserId)| {
			trace!("Removing key: {key:?}");
			txn.del(&self.db.roomuserid_joinedcount, key);

			let reverse_key = (key.1, key.0);

			trace!("Removing reverse key: {reverse_key:?}");
			txn.del(&self.db.userroomid_joinedcount, reverse_key);
		})
		.await;

	self.db
		.roomuserid_knockedcount
		.keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key: (&RoomId, &UserId)| {
			trace!("Removing key: {key:?}");
			txn.del(&self.db.roomuserid_knockedcount, key);

			let reverse_key = (key.1, key.0);

			trace!("Removing reverse key: {reverse_key:?}");
			txn.del(&self.db.userroomid_knockedstate, reverse_key);
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
			txn.del(&self.db.roomuserid_leftcount, key);

			let reverse_key = (key.1, key.0);

			trace!("Removing reverse key: {reverse_key:?}");
			txn.del(&self.db.userroomid_leftstate, reverse_key);
		})
		.await;

	txn.execute();

	Ok(())
}

/// Normalizes either supported cached leave-state representation.
///
/// Imported databases can contain one leave event where this service writes an
/// array of state events. Both shapes are read without rewriting the row, and
/// malformed values are logged before producing an empty vector.
fn state_events<T, U>(room_id: &RoomId, state: &Raw<T>) -> Vec<U>
where
	U: DeserializeOwned + From<Owned<Pdu>>,
{
	match state.json().get().trim_start().as_bytes().first() {
		| Some(b'[') => state
			.deserialize_as_unchecked()
			.inspect_err(
				|e| debug_warn!(%room_id, error = %e, "Unusable cached membership state"),
			)
			.unwrap_or_default(),

		// A foreign row holds the leave event alone; lift it into the array shape.
		| Some(b'{') => state
			.deserialize_as_unchecked()
			.map(|event: Pdu| [event.into_format()].into())
			.inspect_err(|e| debug_warn!(%room_id, error = %e, "Unusable cached leave event"))
			.unwrap_or_default(),

		| _ => Vec::new(),
	}
}
