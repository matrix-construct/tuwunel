mod watch;

#[cfg(test)]
mod tests;

use std::{
	collections::{BTreeMap, btree_map::Entry},
	sync::Arc,
};

use futures::{FutureExt, Stream};
use ruma::{
	DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, UserId,
	api::client::sync::sync_events::v5::{
		ConnId as ConnectionId, ListId, Request, request,
		request::{AccountData, E2EE, Profiles, Receipts, ToDevice, Typing},
	},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
use tuwunel_core::{Result, at, debug, err, implement, is_equal_to, utils::stream::TryIgnore};
use tuwunel_database::{Cbor, Deserialized, Map};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	connections: Connections,
	db: Data,
}

struct Data {
	userdeviceconnid_conn: Arc<Map>,
	todeviceid_events: Arc<Map>,
	userroomid_joined: Arc<Map>,
	userroomid_invitestate: Arc<Map>,
	userroomid_leftstate: Arc<Map>,
	userroomid_knockedstate: Arc<Map>,
	userroomid_notificationcount: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	pduid_pdu: Arc<Map>,
	keychangeid_userid: Arc<Map>,
	profilechangeid_userid: Arc<Map>,
	roomuserdataid_accountdata: Arc<Map>,
	roomusertype_roomuserdataid: Arc<Map>,
	readreceiptid_readreceipt: Arc<Map>,
	userid_lastonetimekeyupdate: Arc<Map>,
	roomuserid_lastnotificationread: Arc<Map>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Connection {
	pub globalsince: u64,
	pub next_batch: u64,
	pub lists: Lists,
	pub extensions: request::Extensions,
	pub subscriptions: Subscriptions,
	pub rooms: Rooms,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct Room {
	pub roomsince: u64,
	#[serde(default)]
	pub config_hash: u64,
}

type Connections = TokioMutex<BTreeMap<ConnectionKey, ConnectionVal>>;
pub type ConnectionVal = Arc<TokioMutex<Connection>>;
pub type ConnectionKey = (OwnedUserId, Option<OwnedDeviceId>, Option<ConnectionId>);

pub type Subscriptions = BTreeMap<OwnedRoomId, request::ListConfig>;
pub type Lists = BTreeMap<ListId, request::List>;
pub type Rooms = BTreeMap<OwnedRoomId, Room>;
type RoomUpdate<'a> = (&'a RoomId, Option<u64>);

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				userdeviceconnid_conn: args.db["userdeviceconnid_conn"].clone(),
				todeviceid_events: args.db["todeviceid_events"].clone(),
				userroomid_joined: args.db["userroomid_joined"].clone(),
				userroomid_invitestate: args.db["userroomid_invitestate"].clone(),
				userroomid_leftstate: args.db["userroomid_leftstate"].clone(),
				userroomid_knockedstate: args.db["userroomid_knockedstate"].clone(),
				userroomid_notificationcount: args.db["userroomid_notificationcount"].clone(),
				userroomid_highlightcount: args.db["userroomid_highlightcount"].clone(),
				pduid_pdu: args.db["pduid_pdu"].clone(),
				keychangeid_userid: args.db["keychangeid_userid"].clone(),
				profilechangeid_userid: args.db["profilechangeid_userid"].clone(),
				roomuserdataid_accountdata: args.db["roomuserdataid_accountdata"].clone(),
				roomusertype_roomuserdataid: args.db["roomusertype_roomuserdataid"].clone(),
				readreceiptid_readreceipt: args.db["readreceiptid_readreceipt"].clone(),
				userid_lastonetimekeyupdate: args.db["userid_lastonetimekeyupdate"].clone(),
				roomuserid_lastnotificationread: args.db["roomuserid_lastnotificationread"]
					.clone(),
			},
			services: args.services.clone(),
			connections: Default::default(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn clear_connections(
	&self,
	user_id: Option<&UserId>,
	device_id: Option<&DeviceId>,
	conn_id: Option<&ConnectionId>,
) {
	self.connections
		.lock()
		.await
		.retain(|(conn_user_id, conn_device_id, conn_conn_id), _| {
			let retain = user_id.is_none_or(is_equal_to!(conn_user_id))
				&& (device_id.is_none() || device_id == conn_device_id.as_deref())
				&& (conn_id.is_none() || conn_id == conn_conn_id.as_ref());

			if !retain {
				self.db
					.userdeviceconnid_conn
					.del((conn_user_id, conn_device_id, conn_conn_id));
			}

			retain
		});
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn drop_connection(&self, key: &ConnectionKey) {
	let mut cache = self.connections.lock().await;

	self.db.userdeviceconnid_conn.del(key);
	cache.remove(key);
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn load_or_init_connection(&self, key: &ConnectionKey) -> ConnectionVal {
	let mut cache = self.connections.lock().await;

	match cache.entry(key.clone()) {
		| Entry::Occupied(val) => val.get().clone(),
		| Entry::Vacant(val) => {
			let conn = self
				.db
				.userdeviceconnid_conn
				.qry(key)
				.boxed()
				.await
				.deserialized::<Cbor<_>>()
				.map(at!(0))
				.map(TokioMutex::new)
				.map(Arc::new)
				.unwrap_or_default();

			val.insert(conn).clone()
		},
	}
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn load_connection(&self, key: &ConnectionKey) -> Result<ConnectionVal> {
	let mut cache = self.connections.lock().await;

	match cache.entry(key.clone()) {
		| Entry::Occupied(val) => Ok(val.get().clone()),
		| Entry::Vacant(val) => self
			.db
			.userdeviceconnid_conn
			.qry(key)
			.await
			.deserialized::<Cbor<_>>()
			.map(at!(0))
			.map(TokioMutex::new)
			.map(Arc::new)
			.map(|conn| val.insert(conn).clone()),
	}
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn get_loaded_connection(&self, key: &ConnectionKey) -> Result<ConnectionVal> {
	self.connections
		.lock()
		.await
		.get(key)
		.cloned()
		.ok_or_else(|| err!(Request(NotFound("Connection not found."))))
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
pub async fn list_loaded_connections(&self) -> Vec<ConnectionKey> {
	self.connections
		.lock()
		.await
		.keys()
		.cloned()
		.collect()
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
pub fn list_stored_connections(&self) -> impl Stream<Item = ConnectionKey> {
	self.db.userdeviceconnid_conn.keys().ignore_err()
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
pub async fn is_connection_loaded(&self, key: &ConnectionKey) -> bool {
	self.connections.lock().await.contains_key(key)
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
pub async fn is_connection_stored(&self, key: &ConnectionKey) -> bool {
	self.db.userdeviceconnid_conn.contains(key).await
}

#[inline]
pub fn into_connection_key<U, D, C>(
	user_id: U,
	device_id: Option<D>,
	conn_id: Option<C>,
) -> ConnectionKey
where
	U: Into<OwnedUserId>,
	D: Into<OwnedDeviceId>,
	C: Into<ConnectionId>,
{
	(user_id.into(), device_id.map(Into::into), conn_id.map(Into::into))
}

#[implement(Connection)]
#[tracing::instrument(level = "debug", skip(self, service))]
pub fn store(&self, service: &Service, key: &ConnectionKey) {
	service
		.db
		.userdeviceconnid_conn
		.put(key, Cbor(self));

	debug!(
		since = %self.globalsince,
		next_batch = %self.next_batch,
		"Persisted connection state"
	);
}

#[implement(Connection)]
#[tracing::instrument(level = "debug", skip(self))]
pub fn update_rooms_prologue(&mut self, retard_since: Option<u64>) {
	self.rooms.values_mut().for_each(|room| {
		if let Some(retard_since) = retard_since
			&& room.roomsince > retard_since
		{
			room.roomsince = retard_since;
			room.config_hash = 0;
		}
	});
}

/// Advance the per-room cursor for each complete bounded room range.
///
/// `roomsince` is the lower bound of every content query for its room. Only
/// rooms whose complete range was safely assembled may advance. A failed room
/// keeps its cursor and retries the same range after a later wake.
#[implement(Connection)]
#[tracing::instrument(level = "debug", skip_all)]
pub fn update_rooms_epilogue<'a, Complete>(&mut self, complete: Complete)
where
	Complete: Iterator<Item = RoomUpdate<'a>> + Send + 'a,
{
	let next_batch = self.next_batch;
	complete.for_each(|(room_id, config_hash)| {
		let room = self.rooms.entry(room_id.into()).or_default();

		room.roomsince = next_batch;
		if let Some(config_hash) = config_hash {
			room.config_hash = config_hash;
		}
	});
}

#[implement(Connection)]
#[tracing::instrument(level = "debug", skip_all)]
pub fn update_cache(&mut self, request: &Request) -> bool {
	let lists_changed = Self::update_cache_lists(request, self);
	let subscriptions_changed = Self::update_cache_subscriptions(request, self);

	Self::update_cache_extensions(request, self);

	lists_changed || subscriptions_changed
}

#[implement(Connection)]
fn update_cache_lists(request: &Request, cached: &mut Self) -> bool {
	request
		.lists
		.iter()
		.fold(false, |changed, (list_id, request_list)| {
			let list_changed = match cached.lists.get_mut(list_id) {
				| Some(cached_list) => Self::update_cache_list(request_list, cached_list),
				| None => {
					cached
						.lists
						.insert(list_id.clone(), request_list.clone());

					true
				},
			};

			changed | list_changed
		})
}

#[implement(Connection)]
fn update_cache_list(request: &request::List, cached: &mut request::List) -> bool {
	let ranges_changed = request.ranges != cached.ranges;
	let timeline_limit_changed =
		request.room_details.timeline_limit != cached.room_details.timeline_limit;

	let required_state_changed = !request.room_details.required_state.is_empty()
		&& request.room_details.required_state != cached.room_details.required_state;

	let filters_changed = request.filters.as_ref().is_some_and(|request| {
		cached
			.filters
			.as_ref()
			.is_none_or(|cached| !list_filters_are_equal(request, cached))
	});

	let changed =
		ranges_changed || timeline_limit_changed || required_state_changed || filters_changed;

	if ranges_changed {
		cached.ranges.clone_from(&request.ranges);
	}

	cached.room_details.timeline_limit = request.room_details.timeline_limit;

	if required_state_changed {
		cached
			.room_details
			.required_state
			.clone_from(&request.room_details.required_state);
	}

	if filters_changed {
		cached.filters.clone_from(&request.filters);
	}

	changed
}

#[implement(Connection)]
fn update_cache_subscriptions(request: &Request, cached: &mut Self) -> bool {
	let changed = !subscriptions_are_equal(&request.room_subscriptions, &cached.subscriptions);

	if changed {
		cached
			.subscriptions
			.clone_from(&request.room_subscriptions);
	}

	changed
}

fn subscriptions_are_equal(request: &Subscriptions, cached: &Subscriptions) -> bool {
	request.len() == cached.len()
		&& request
			.iter()
			.zip(cached)
			.all(|(request, cached)| {
				request.0 == cached.0 && list_config_is_equal(request.1, cached.1)
			})
}

fn list_config_is_equal(request: &request::ListConfig, cached: &request::ListConfig) -> bool {
	request.timeline_limit == cached.timeline_limit
		&& request.required_state == cached.required_state
}

fn list_filters_are_equal(request: &request::ListFilters, cached: &request::ListFilters) -> bool {
	request.is_dm == cached.is_dm
		&& request.is_encrypted == cached.is_encrypted
		&& request.is_invite == cached.is_invite
		&& request.room_types == cached.room_types
		&& request.not_room_types == cached.not_room_types
		&& request.tags == cached.tags
		&& request.not_tags == cached.not_tags
		&& request.spaces == cached.spaces
}

#[implement(Connection)]
fn update_cache_extensions(request: &Request, cached: &mut Self) {
	let request = &request.extensions;
	let cached = &mut cached.extensions;

	Self::update_cache_account_data(&request.account_data, &mut cached.account_data);
	Self::update_cache_receipts(&request.receipts, &mut cached.receipts);
	Self::update_cache_typing(&request.typing, &mut cached.typing);
	Self::update_cache_to_device(&request.to_device, &mut cached.to_device);
	Self::update_cache_e2ee(&request.e2ee, &mut cached.e2ee);
	Self::update_cache_profiles(&request.profiles, &mut cached.profiles);
}

#[implement(Connection)]
fn update_cache_account_data(request: &AccountData, cached: &mut AccountData) {
	some_or_sticky(request.enabled.as_ref(), &mut cached.enabled);
	some_or_sticky(request.lists.as_ref(), &mut cached.lists);
	some_or_sticky(request.rooms.as_ref(), &mut cached.rooms);
}

#[implement(Connection)]
fn update_cache_receipts(request: &Receipts, cached: &mut Receipts) {
	some_or_sticky(request.enabled.as_ref(), &mut cached.enabled);
	some_or_sticky(request.rooms.as_ref(), &mut cached.rooms);
	some_or_sticky(request.lists.as_ref(), &mut cached.lists);
}

#[implement(Connection)]
fn update_cache_typing(request: &Typing, cached: &mut Typing) {
	some_or_sticky(request.enabled.as_ref(), &mut cached.enabled);
	some_or_sticky(request.rooms.as_ref(), &mut cached.rooms);
	some_or_sticky(request.lists.as_ref(), &mut cached.lists);
}

#[implement(Connection)]
fn update_cache_to_device(request: &ToDevice, cached: &mut ToDevice) {
	some_or_sticky(request.enabled.as_ref(), &mut cached.enabled);
	cached.since.clone_from(&request.since);
}

#[implement(Connection)]
fn update_cache_profiles(request: &Profiles, cached: &mut Profiles) {
	some_or_sticky(request.enabled.as_ref(), &mut cached.enabled);
	some_or_sticky(request.rooms.as_ref(), &mut cached.rooms);
	some_or_sticky(request.lists.as_ref(), &mut cached.lists);
	some_or_sticky(request.fields.as_ref(), &mut cached.fields);
}

#[implement(Connection)]
fn update_cache_e2ee(request: &E2EE, cached: &mut E2EE) {
	some_or_sticky(request.enabled.as_ref(), &mut cached.enabled);
}

fn some_or_sticky<T: Clone>(target: Option<&T>, cached: &mut Option<T>) {
	if let Some(target) = target {
		cached.replace(target.clone());
	}
}
