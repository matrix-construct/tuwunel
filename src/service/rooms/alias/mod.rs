mod remote;

use std::sync::Arc;

use futures::{Stream, StreamExt, TryFutureExt};
use ruma::{
	OwnedRoomId, OwnedServerName, OwnedUserId, RoomAliasId, RoomId, RoomOrAliasId, UserId,
	events::{
		StateEventType,
		room::power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
	},
};
use tuwunel_core::{
	Err, Result, Server, err,
	utils::{ReadyExt, stream::TryIgnore},
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Map};

use crate::{Dep, admin, appservice, appservice::RegistrationInfo, globals, rooms, sending};

pub struct Service {
	db: Data,
	services: Services,
}

struct Data {
	alias_userid: Arc<Map>,
	alias_roomid: Arc<Map>,
	aliasid_alias: Arc<Map>,
}

struct Services {
	server: Arc<Server>,
	admin: Dep<admin::Service>,
	appservice: Dep<appservice::Service>,
	globals: Dep<globals::Service>,
	sending: Dep<sending::Service>,
	state_accessor: Dep<rooms::state_accessor::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				alias_userid: args.db["alias_userid"].clone(),
				alias_roomid: args.db["alias_roomid"].clone(),
				aliasid_alias: args.db["aliasid_alias"].clone(),
			},
			services: Services {
				server: args.server.clone(),
				admin: args.depend::<admin::Service>("admin"),
				appservice: args.depend::<appservice::Service>("appservice"),
				globals: args.depend::<globals::Service>("globals"),
				sending: args.depend::<sending::Service>("sending"),
				state_accessor: args
					.depend::<rooms::state_accessor::Service>("rooms::state_accessor"),
			},
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

impl Service {
	#[tracing::instrument(skip(self))]
	pub fn set_alias(
		&self,
		alias: &RoomAliasId,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<()> {
		if alias == self.services.globals.admin_alias
			&& user_id != self.services.globals.server_user
		{
			return Err!(Request(Forbidden("Only the server user can set this alias")));
		}

		// Comes first as we don't want a stuck alias
		self.db
			.alias_userid
			.insert(alias.alias().as_bytes(), user_id.as_bytes());

		self.db
			.alias_roomid
			.insert(alias.alias().as_bytes(), room_id.as_bytes());

		let mut aliasid = room_id.as_bytes().to_vec();
		aliasid.push(0xFF);
		aliasid.extend_from_slice(&self.services.globals.next_count()?.to_be_bytes());
		self.db
			.aliasid_alias
			.insert(&aliasid, alias.as_bytes());

		Ok(())
	}

	#[tracing::instrument(skip(self))]
	pub async fn remove_alias(&self, alias: &RoomAliasId, user_id: &UserId) -> Result<()> {
		if !self.user_can_remove_alias(alias, user_id).await? {
			return Err!(Request(Forbidden("User is not permitted to remove this alias.")));
		}

		let alias = alias.alias();
		let Ok(room_id) = self.db.alias_roomid.get(&alias).await else {
			return Err!(Request(NotFound("Alias does not exist or is invalid.")));
		};

		let prefix = (&room_id, Interfix);
		self.db
			.aliasid_alias
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| self.db.aliasid_alias.remove(key))
			.await;

		self.db.alias_roomid.remove(alias.as_bytes());
		self.db.alias_userid.remove(alias.as_bytes());

		Ok(())
	}

	#[inline]
	pub async fn resolve(&self, room: &RoomOrAliasId) -> Result<OwnedRoomId> {
		self.resolve_with_servers(room, None)
			.await
			.map(|(room_id, _)| room_id)
	}

	pub async fn resolve_with_servers(
		&self,
		room: &RoomOrAliasId,
		servers: Option<Vec<OwnedServerName>>,
	) -> Result<(OwnedRoomId, Vec<OwnedServerName>)> {
		if room.is_room_id() {
			let room_id: &RoomId = room.try_into().expect("valid RoomId");
			Ok((room_id.to_owned(), servers.unwrap_or_default()))
		} else {
			let alias: &RoomAliasId = room.try_into().expect("valid RoomAliasId");
			self.resolve_alias(alias, servers).await
		}
	}

	#[tracing::instrument(skip(self), name = "resolve")]
	pub async fn resolve_alias(
		&self,
		room_alias: &RoomAliasId,
		servers: Option<Vec<OwnedServerName>>,
	) -> Result<(OwnedRoomId, Vec<OwnedServerName>)> {
		let server_name = room_alias.server_name();
		let server_is_ours = self.services.globals.server_is_ours(server_name);
		let servers_contains_ours = || {
			servers
				.as_ref()
				.is_some_and(|servers| servers.contains(&self.services.server.name))
		};

		if !server_is_ours && !servers_contains_ours() {
			return self
				.remote_resolve(room_alias, servers.unwrap_or_default())
				.await;
		}

		let room_id = match self.resolve_local_alias(room_alias).await {
			| Ok(r) => Some(r),
			| Err(_) => self.resolve_appservice_alias(room_alias).await?,
		};

		room_id.map_or_else(
			|| Err!(Request(NotFound("Room with alias not found."))),
			|room_id| Ok((room_id, Vec::new())),
		)
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn resolve_local_alias(&self, alias: &RoomAliasId) -> Result<OwnedRoomId> {
		self.db
			.alias_roomid
			.get(alias.alias())
			.await
			.deserialized()
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub fn local_aliases_for_room<'a>(
		&'a self,
		room_id: &'a RoomId,
	) -> impl Stream<Item = &RoomAliasId> + Send + 'a {
		let prefix = (room_id, Interfix);
		self.db
			.aliasid_alias
			.stream_prefix(&prefix)
			.ignore_err()
			.map(|(_, alias): (Ignore, &RoomAliasId)| alias)
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub fn all_local_aliases<'a>(&'a self) -> impl Stream<Item = (&RoomId, &str)> + Send + 'a {
		self.db
			.alias_roomid
			.stream()
			.ignore_err()
			.map(|(alias_localpart, room_id): (&str, &RoomId)| (room_id, alias_localpart))
	}

	async fn user_can_remove_alias(&self, alias: &RoomAliasId, user_id: &UserId) -> Result<bool> {
		let room_id = self
			.resolve_local_alias(alias)
			.await
			.map_err(|_| err!(Request(NotFound("Alias not found."))))?;

		let server_user = &self.services.globals.server_user;

		// The creator of an alias can remove it
		if self
            .who_created_alias(alias).await
            .is_ok_and(|user| user == user_id)
            // Server admins can remove any local alias
            || self.services.admin.user_is_admin(user_id).await
            // Always allow the server service account to remove the alias, since there may not be an admin room
            || server_user == user_id
		{
			return Ok(true);
		}

		// Checking whether the user is able to change canonical aliases of the room
		if let Ok(power_levels) = self
			.services
			.state_accessor
			.room_state_get_content::<RoomPowerLevelsEventContent>(
				&room_id,
				&StateEventType::RoomPowerLevels,
				"",
			)
			.map_ok(RoomPowerLevels::from)
			.await
		{
			return Ok(
				power_levels.user_can_send_state(user_id, StateEventType::RoomCanonicalAlias)
			);
		}

		// If there is no power levels event, only the room creator can change
		// canonical aliases
		if let Ok(event) = self
			.services
			.state_accessor
			.room_state_get(&room_id, &StateEventType::RoomCreate, "")
			.await
		{
			return Ok(event.sender == user_id);
		}

		Err!(Database("Room has no m.room.create event"))
	}

	async fn who_created_alias(&self, alias: &RoomAliasId) -> Result<OwnedUserId> {
		self.db
			.alias_userid
			.get(alias.alias())
			.await
			.deserialized()
	}

	async fn resolve_appservice_alias(
		&self,
		room_alias: &RoomAliasId,
	) -> Result<Option<OwnedRoomId>> {
		use ruma::api::appservice::query::query_room_alias;

		for appservice in self.services.appservice.read().await.values() {
			if appservice.aliases.is_match(room_alias.as_str())
				&& matches!(
					self.services
						.sending
						.send_appservice_request(
							appservice.registration.clone(),
							query_room_alias::v1::Request { room_alias: room_alias.to_owned() },
						)
						.await,
					Ok(Some(_opt_result))
				) {
				return self
					.resolve_local_alias(room_alias)
					.await
					.map_err(|_| err!(Request(NotFound("Room does not exist."))))
					.map(Some);
			}
		}

		Ok(None)
	}

	pub async fn appservice_checks(
		&self,
		room_alias: &RoomAliasId,
		appservice_info: &Option<RegistrationInfo>,
	) -> Result<()> {
		if !self
			.services
			.globals
			.server_is_ours(room_alias.server_name())
		{
			return Err!(Request(InvalidParam("Alias is from another server.")));
		}

		if let Some(info) = appservice_info {
			if !info.aliases.is_match(room_alias.as_str()) {
				return Err!(Request(Exclusive("Room alias is not in namespace.")));
			}
		} else if self
			.services
			.appservice
			.is_exclusive_alias(room_alias)
			.await
		{
			return Err!(Request(Exclusive("Room alias reserved by appservice.")));
		}

		Ok(())
	}
}
