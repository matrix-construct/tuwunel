use std::{
	collections::{HashMap, HashSet},
	sync::{Arc, RwLock},
};

use futures::{Stream, StreamExt, future::join5, pin_mut, stream::iter};
use itertools::Itertools;
use ruma::{
	OwnedRoomId, OwnedServerName, RoomId, ServerName, UserId,
	events::{
		AnyStrippedStateEvent, AnySyncStateEvent, GlobalAccountDataEventType,
		RoomAccountDataEventType, StateEventType,
		direct::DirectEvent,
		room::{
			create::RoomCreateEventContent,
			member::{MembershipState, RoomMemberEventContent},
			power_levels::RoomPowerLevelsEventContent,
		},
	},
	int,
	serde::Raw,
};
use tuwunel_core::{
	Result, is_not_empty,
	result::LogErr,
	utils::{ReadyExt, StreamTools, stream::TryIgnore},
	warn,
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json, Map, serialize_key};

use crate::{Dep, account_data, appservice::RegistrationInfo, config, globals, rooms, users};

pub struct Service {
	appservice_in_room_cache: AppServiceInRoomCache,
	services: Services,
	db: Data,
}

struct Services {
	account_data: Dep<account_data::Service>,
	config: Dep<config::Service>,
	globals: Dep<globals::Service>,
	metadata: Dep<rooms::metadata::Service>,
	state_accessor: Dep<rooms::state_accessor::Service>,
	users: Dep<users::Service>,
}

struct Data {
	roomid_invitedcount: Arc<Map>,
	roomid_inviteviaservers: Arc<Map>,
	roomid_joinedcount: Arc<Map>,
	roomserverids: Arc<Map>,
	roomuserid_invitecount: Arc<Map>,
	roomuserid_joined: Arc<Map>,
	roomuserid_leftcount: Arc<Map>,
	roomuserid_knockedcount: Arc<Map>,
	roomuseroncejoinedids: Arc<Map>,
	serverroomids: Arc<Map>,
	userroomid_invitestate: Arc<Map>,
	userroomid_joined: Arc<Map>,
	userroomid_leftstate: Arc<Map>,
	userroomid_knockedstate: Arc<Map>,
}

type AppServiceInRoomCache = RwLock<HashMap<OwnedRoomId, HashMap<String, bool>>>;
type StrippedStateEventItem = (OwnedRoomId, Vec<Raw<AnyStrippedStateEvent>>);
type SyncStateEventItem = (OwnedRoomId, Vec<Raw<AnySyncStateEvent>>);

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			appservice_in_room_cache: RwLock::new(HashMap::new()),
			services: Services {
				account_data: args.depend::<account_data::Service>("account_data"),
				config: args.depend::<config::Service>("config"),
				globals: args.depend::<globals::Service>("globals"),
				metadata: args.depend::<rooms::metadata::Service>("rooms::metadata"),
				state_accessor: args
					.depend::<rooms::state_accessor::Service>("rooms::state_accessor"),
				users: args.depend::<users::Service>("users"),
			},
			db: Data {
				roomid_invitedcount: args.db["roomid_invitedcount"].clone(),
				roomid_inviteviaservers: args.db["roomid_inviteviaservers"].clone(),
				roomid_joinedcount: args.db["roomid_joinedcount"].clone(),
				roomserverids: args.db["roomserverids"].clone(),
				roomuserid_invitecount: args.db["roomuserid_invitecount"].clone(),
				roomuserid_joined: args.db["roomuserid_joined"].clone(),
				roomuserid_leftcount: args.db["roomuserid_leftcount"].clone(),
				roomuserid_knockedcount: args.db["roomuserid_knockedcount"].clone(),
				roomuseroncejoinedids: args.db["roomuseroncejoinedids"].clone(),
				serverroomids: args.db["serverroomids"].clone(),
				userroomid_invitestate: args.db["userroomid_invitestate"].clone(),
				userroomid_joined: args.db["userroomid_joined"].clone(),
				userroomid_leftstate: args.db["userroomid_leftstate"].clone(),
				userroomid_knockedstate: args.db["userroomid_knockedstate"].clone(),
			},
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

impl Service {
	/// Update current membership data.
	#[tracing::instrument(
		level = "debug",
		skip_all,
		fields(
			%room_id,
			%user_id,
			%sender,
			?membership_event,
		),
	)]
	#[allow(clippy::too_many_arguments)]
	pub async fn update_membership(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		membership_event: RoomMemberEventContent,
		sender: &UserId,
		last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
		invite_via: Option<Vec<OwnedServerName>>,
		update_joined_count: bool,
	) -> Result<()> {
		let membership = membership_event.membership;

		// Keep track what remote users exist by adding them as "deactivated" users
		//
		// TODO: use futures to update remote profiles without blocking the membership
		// update
		#[allow(clippy::collapsible_if)]
		if !self.services.globals.user_is_local(user_id) {
			if !self.services.users.exists(user_id).await {
				self.services.users.create(user_id, None)?;
			}

			/*
			// Try to update our local copy of the user if ours does not match
			if ((self.services.users.displayname(user_id)? != membership_event.displayname)
				|| (self.services.users.avatar_url(user_id)? != membership_event.avatar_url)
				|| (self.services.users.blurhash(user_id)? != membership_event.blurhash))
				&& (membership != MembershipState::Leave)
			{
				let response = self.services
					.sending
					.send_federation_request(
						user_id.server_name(),
						federation::query::get_profile_information::v1::Request {
							user_id: user_id.into(),
							field: None, // we want the full user's profile to update locally too
						},
					)
					.await;

				self.services.users.set_displayname(user_id, response.displayname.clone()).await?;
				self.services.users.set_avatar_url(user_id, response.avatar_url).await?;
				self.services.users.set_blurhash(user_id, response.blurhash).await?;
			};
			*/
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
						// Copy user settings from predecessor to the current room:
						// - Push rules
						//
						// TODO: finish this once push rules are implemented.
						//
						// let mut push_rules_event_content: PushRulesEvent = account_data
						//     .get(
						//         None,
						//         user_id,
						//         EventType::PushRules,
						//     )?;
						//
						// NOTE: find where `predecessor.room_id` match
						//       and update to `room_id`.
						//
						// account_data
						//     .update(
						//         None,
						//         user_id,
						//         EventType::PushRules,
						//         &push_rules_event_content,
						//         globals,
						//     )
						//     .ok();

						// Copy old tags to new room
						if let Ok(tag_event) = self
							.services
							.account_data
							.get_room(
								&predecessor.room_id,
								user_id,
								RoomAccountDataEventType::Tag,
							)
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
							.get_global::<DirectEvent>(
								user_id,
								GlobalAccountDataEventType::Direct,
							)
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

				self.mark_as_joined(user_id, room_id);
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

				self.mark_as_invited(user_id, room_id, last_state, invite_via)
					.await;
			},
			| MembershipState::Leave | MembershipState::Ban => {
				self.mark_as_left(user_id, room_id);

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

	#[tracing::instrument(level = "trace", skip_all)]
	pub async fn appservice_in_room(
		&self,
		room_id: &RoomId,
		appservice: &RegistrationInfo,
	) -> bool {
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

	/// Direct DB function to directly mark a user as joined. It is not
	/// recommended to use this directly. You most likely should use
	/// `update_membership` instead
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn mark_as_joined(&self, user_id: &UserId, room_id: &RoomId) {
		let userroom_id = (user_id, room_id);
		let userroom_id = serialize_key(userroom_id).expect("failed to serialize userroom_id");

		let roomuser_id = (room_id, user_id);
		let roomuser_id = serialize_key(roomuser_id).expect("failed to serialize roomuser_id");

		self.db.userroomid_joined.insert(&userroom_id, []);
		self.db.roomuserid_joined.insert(&roomuser_id, []);

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
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn mark_as_left(&self, user_id: &UserId, room_id: &RoomId) {
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
			.raw_aput::<8, _, _>(&roomuser_id, self.services.globals.next_count().unwrap());

		self.db.userroomid_joined.remove(&userroom_id);
		self.db.roomuserid_joined.remove(&roomuser_id);

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
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn mark_as_knocked(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
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
			.raw_aput::<8, _, _>(&roomuser_id, self.services.globals.next_count().unwrap());

		self.db.userroomid_joined.remove(&userroom_id);
		self.db.roomuserid_joined.remove(&roomuser_id);

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
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn forget(&self, room_id: &RoomId, user_id: &UserId) {
		let userroom_id = (user_id, room_id);
		let roomuser_id = (room_id, user_id);

		self.db.userroomid_leftstate.del(userroom_id);
		self.db.roomuserid_leftcount.del(roomuser_id);
	}

	/// Returns an iterator of all servers participating in this room.
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

	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn server_in_room<'a>(
		&'a self,
		server: &'a ServerName,
		room_id: &'a RoomId,
	) -> bool {
		let key = (server, room_id);
		self.db.serverroomids.qry(&key).await.is_ok()
	}

	/// Returns an iterator of all rooms a server participates in (as far as we
	/// know).
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
	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn server_sees_user(&self, server: &ServerName, user_id: &UserId) -> bool {
		self.server_rooms(server)
			.any(|room_id| self.is_joined(user_id, room_id))
			.await
	}

	/// Returns true if user_a and user_b share at least one room.
	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn user_sees_user(&self, user_a: &UserId, user_b: &UserId) -> bool {
		let get_shared_rooms = self.get_shared_rooms(user_a, user_b);

		pin_mut!(get_shared_rooms);
		get_shared_rooms.next().await.is_some()
	}

	/// List the rooms common between two users
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn get_shared_rooms<'a>(
		&'a self,
		user_a: &'a UserId,
		user_b: &'a UserId,
	) -> impl Stream<Item = &RoomId> + Send + 'a {
		use tuwunel_core::utils::set;

		let a = self.rooms_joined(user_a);
		let b = self.rooms_joined(user_b);
		set::intersection_sorted_stream2(a, b)
	}

	/// Returns an iterator of all joined members of a room.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn room_members<'a>(
		&'a self,
		room_id: &'a RoomId,
	) -> impl Stream<Item = &UserId> + Send + 'a {
		let prefix = (room_id, Interfix);
		self.db
			.roomuserid_joined
			.keys_prefix(&prefix)
			.ignore_err()
			.map(|(_, user_id): (Ignore, &UserId)| user_id)
	}

	/// Returns the number of users which are currently in a room
	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn room_joined_count(&self, room_id: &RoomId) -> Result<u64> {
		self.db
			.roomid_joinedcount
			.get(room_id)
			.await
			.deserialized()
	}

	#[tracing::instrument(skip(self), level = "debug")]
	/// Returns an iterator of all our local users in the room, even if they're
	/// deactivated/guests
	pub fn local_users_in_room<'a>(
		&'a self,
		room_id: &'a RoomId,
	) -> impl Stream<Item = &UserId> + Send + 'a {
		self.room_members(room_id)
			.ready_filter(|user| self.services.globals.user_is_local(user))
	}

	/// Returns an iterator of all our local joined users in a room who are
	/// active (not deactivated, not guest)
	#[tracing::instrument(skip(self), level = "trace")]
	pub fn active_local_users_in_room<'a>(
		&'a self,
		room_id: &'a RoomId,
	) -> impl Stream<Item = &UserId> + Send + 'a {
		self.local_users_in_room(room_id)
			.filter(|user| self.services.users.is_active(user))
	}

	/// Returns the number of users which are currently invited to a room
	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn room_invited_count(&self, room_id: &RoomId) -> Result<u64> {
		self.db
			.roomid_invitedcount
			.get(room_id)
			.await
			.deserialized()
	}

	/// Returns an iterator over all User IDs who ever joined a room.
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

	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn get_invite_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<u64> {
		let key = (room_id, user_id);
		self.db
			.roomuserid_invitecount
			.qry(&key)
			.await
			.deserialized()
	}

	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn get_knock_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<u64> {
		let key = (room_id, user_id);
		self.db
			.roomuserid_knockedcount
			.qry(&key)
			.await
			.deserialized()
	}

	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn get_left_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<u64> {
		let key = (room_id, user_id);
		self.db
			.roomuserid_leftcount
			.qry(&key)
			.await
			.deserialized()
	}

	/// Returns an iterator over all rooms this user joined.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn rooms_joined<'a>(
		&'a self,
		user_id: &'a UserId,
	) -> impl Stream<Item = &RoomId> + Send + 'a {
		self.db
			.userroomid_joined
			.keys_raw_prefix(user_id)
			.ignore_err()
			.map(|(_, room_id): (Ignore, &RoomId)| room_id)
	}

	/// Returns an iterator over all rooms a user was invited to.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn rooms_invited<'a>(
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
			.map(|(room_id, state)| Ok((room_id, state.deserialize_as()?)))
			.ignore_err()
	}

	/// Returns an iterator over all rooms a user is currently knocking.
	#[tracing::instrument(skip(self), level = "trace")]
	pub fn rooms_knocked<'a>(
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
			.map(|(room_id, state)| Ok((room_id, state.deserialize_as()?)))
			.ignore_err()
	}

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
				val.deserialize_as().map_err(Into::into)
			})
	}

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
				val.deserialize_as().map_err(Into::into)
			})
	}

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
				val.deserialize_as().map_err(Into::into)
			})
	}

	/// Returns an iterator over all rooms a user left.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn rooms_left<'a>(
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
			.map(|(room_id, state)| Ok((room_id, state.deserialize_as()?)))
			.ignore_err()
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> bool {
		let key = (user_id, room_id);
		self.db
			.roomuseroncejoinedids
			.qry(&key)
			.await
			.is_ok()
	}

	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn is_joined<'a>(&'a self, user_id: &'a UserId, room_id: &'a RoomId) -> bool {
		let key = (user_id, room_id);
		self.db.userroomid_joined.qry(&key).await.is_ok()
	}

	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn is_knocked<'a>(&'a self, user_id: &'a UserId, room_id: &'a RoomId) -> bool {
		let key = (user_id, room_id);
		self.db
			.userroomid_knockedstate
			.qry(&key)
			.await
			.is_ok()
	}

	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> bool {
		let key = (user_id, room_id);
		self.db
			.userroomid_invitestate
			.qry(&key)
			.await
			.is_ok()
	}

	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> bool {
		let key = (user_id, room_id);
		self.db
			.userroomid_leftstate
			.qry(&key)
			.await
			.is_ok()
	}

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

	#[tracing::instrument(skip(self), level = "debug")]
	pub fn servers_invite_via<'a>(
		&'a self,
		room_id: &'a RoomId,
	) -> impl Stream<Item = &ServerName> + Send + 'a {
		type KeyVal<'a> = (Ignore, Vec<&'a ServerName>);

		self.db
			.roomid_inviteviaservers
			.stream_raw_prefix(room_id)
			.ignore_err()
			.map(|(_, servers): KeyVal<'_>| *servers.last().expect("at least one server"))
	}

	/// Gets up to five servers that are likely to be in the room in the
	/// distant future.
	///
	/// See <https://spec.matrix.org/latest/appendices/#routing>
	#[tracing::instrument(skip(self), level = "trace")]
	pub async fn servers_route_via(&self, room_id: &RoomId) -> Result<Vec<OwnedServerName>> {
		let most_powerful_user_server = self
			.services
			.state_accessor
			.room_state_get_content(room_id, &StateEventType::RoomPowerLevels, "")
			.await
			.map(|content: RoomPowerLevelsEventContent| {
				content
					.users
					.iter()
					.max_by_key(|(_, power)| *power)
					.and_then(|x| (x.1 >= &int!(50)).then_some(x))
					.map(|(user, _power)| user.server_name().to_owned())
			});

		let mut servers: Vec<OwnedServerName> = self
			.room_members(room_id)
			.counts_by(|user| user.server_name().to_owned())
			.await
			.into_iter()
			.sorted_by_key(|(_, users)| *users)
			.map(|(server, _)| server)
			.rev()
			.take(5)
			.collect();

		if let Ok(Some(server)) = most_powerful_user_server {
			servers.insert(0, server);
			servers.truncate(5);
		}

		Ok(servers)
	}

	pub fn get_appservice_in_room_cache_usage(&self) -> (usize, usize) {
		let cache = self
			.appservice_in_room_cache
			.read()
			.expect("locked");

		(cache.len(), cache.capacity())
	}

	#[tracing::instrument(level = "debug", skip_all)]
	pub fn clear_appservice_in_room_cache(&self) {
		self.appservice_in_room_cache
			.write()
			.expect("locked")
			.clear();
	}

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
			.roomuserid_knockedcount
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

	#[tracing::instrument(level = "debug", skip(self))]
	fn mark_as_once_joined(&self, user_id: &UserId, room_id: &RoomId) {
		let key = (user_id, room_id);
		self.db.roomuseroncejoinedids.put_raw(key, []);
	}

	#[tracing::instrument(level = "debug", skip(self, last_state, invite_via))]
	pub async fn mark_as_invited(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
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
			.raw_aput::<8, _, _>(&roomuser_id, self.services.globals.next_count().unwrap());

		self.db.userroomid_joined.remove(&userroom_id);
		self.db.roomuserid_joined.remove(&roomuser_id);

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

	#[tracing::instrument(level = "debug", skip(self, servers))]
	pub async fn add_servers_invite_via(&self, room_id: &RoomId, servers: Vec<OwnedServerName>) {
		let mut servers: Vec<_> = self
			.servers_invite_via(room_id)
			.map(ToOwned::to_owned)
			.chain(iter(servers.into_iter()))
			.collect()
			.await;

		servers.sort_unstable();
		servers.dedup();

		let servers = servers
			.iter()
			.map(|server| server.as_bytes())
			.collect_vec()
			.join(&[0xFF][..]);

		self.db
			.roomid_inviteviaservers
			.insert(room_id.as_bytes(), &servers);
	}
}
