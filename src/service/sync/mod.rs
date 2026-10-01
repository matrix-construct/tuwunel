mod change_index;
mod watch;

use std::{
	collections::{BTreeMap, btree_map::Entry},
	sync::Arc,
};

use async_trait::async_trait;
use futures::{FutureExt, Stream, StreamExt, pin_mut};
use ruma::{
	DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, UserId,
	api::client::sync::sync_events::v5::{
		ConnId as ConnectionId, ListId, Request, request,
		request::{AccountData, E2EE, Receipts, ToDevice, Typing},
	},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
use tuwunel_core::{
	Result, at, debug, err, implement, is_equal_to, utils::stream::TryIgnore, warn,
};
use tuwunel_database::{Cbor, Deserialized, Map};

use self::change_index::{ChangeIndex, Layout};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	connections: Connections,
	db: Data,
	changes: Arc<ChangeIndex>,
	/// False when a change hook could not be installed; the index then stays
	/// off.
	hooks_installed: bool,
}

/// Every map whose writes can change what an incremental v3 `/sync` returns:
/// the maps `watch.rs` watches, plus the private-read maps that
/// `load_joined_room` reads but nothing watches. Typing is in-memory and read
/// from the typing service directly. Keep in step with `watch.rs`.
const INDEXED_MAPS: &[(&str, Layout)] = &[
	("userroomid_joined", Layout::User),
	("userroomid_invitestate", Layout::User),
	("userroomid_leftstate", Layout::User),
	("userroomid_knockedstate", Layout::User),
	("userroomid_notificationcount", Layout::User),
	("userroomid_highlightcount", Layout::User),
	("todeviceid_events", Layout::User),
	("userid_lastonetimekeyupdate", Layout::User),
	("keychangeid_userid", Layout::UserOrRoom),
	("roomusertype_roomuserdataid", Layout::SecondIsUser),
	("roomuserdataid_accountdata", Layout::SecondIsUser),
	("roomuserid_lastnotificationread", Layout::SecondIsUser),
	("roomuserid_privateread", Layout::SecondIsUser),
	("roomuserid_lastprivatereadupdate", Layout::SecondIsUser),
	("readreceiptid_readreceipt", Layout::Room),
	("pduid_pdu", Layout::ShortRoom),
];

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
}

type Connections = TokioMutex<BTreeMap<ConnectionKey, ConnectionVal>>;
pub type ConnectionVal = Arc<TokioMutex<Connection>>;
pub type ConnectionKey = (OwnedUserId, Option<OwnedDeviceId>, Option<ConnectionId>);

pub type Subscriptions = BTreeMap<OwnedRoomId, request::ListConfig>;
pub type Lists = BTreeMap<ListId, request::List>;
pub type Rooms = BTreeMap<OwnedRoomId, Room>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let changes = Arc::new(ChangeIndex::default());
		let mut hooks_installed = true;
		for &(name, layout) in INDEXED_MAPS {
			let index = changes.clone();
			let services = args.services.clone();
			hooks_installed &= args.db[name].set_change_hook(Box::new(move |key| {
				if !index.is_enabled() {
					return;
				}
				// Enabled only after startup, so the services are always there;
				// if they somehow are not, stamp the maximum: rebuild, never skip.
				let count = services
					.try_get()
					.map_or(u64::MAX, |s| s.globals.pending_count().end);
				index.record(layout, key, count);
			}));
		}

		Ok(Arc::new(Self {
			changes,
			hooks_installed,
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

	/// Switch the change index on once startup (migrations included) is done.
	///
	/// Only in a process that started as primary. A replica applies the
	/// primary's WAL with raw batch writes that never reach the hooks, and a
	/// replica promoted in place keeps that history in memory, so it rebuilds
	/// every poll as before until the restart that follows promotion.
	async fn worker(self: Arc<Self>) -> Result {
		if self.services.config.rocksdb_primary_url.is_some() {
			return Ok(());
		}
		if !self.hooks_installed {
			warn!("sync change index disabled: a map already had a change hook");
			return Ok(());
		}
		let globals = self.services.globals.clone();
		self.changes
			.enable(|| globals.pending_count().end);
		debug!("sync change index enabled");
		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Switch the change index off for the rest of this process's life. Called on
/// demotion: a replica's writes arrive as raw WAL frames that never stamp it.
#[implement(Service)]
pub fn retire_change_index(&self) { self.changes.retire(); }

/// Whether anything an incremental v3 `/sync` for `user_id` reads may have
/// changed after `since`. `false` means a rebuild from `since` would come back
/// empty, so the caller can skip it and wait on the watchers instead.
///
/// Answers `true` ("rebuild") whenever it cannot vouch otherwise: index off or
/// too new for `since`, presence on (not indexed), or a room without a short
/// id.
#[implement(Service)]
pub async fn changed_since(&self, user_id: &UserId, since: u64) -> bool {
	if self.services.config.allow_local_presence || !self.changes.covers(since) {
		return true;
	}
	if self
		.changes
		.user_changed(user_id.as_bytes(), since)
	{
		return true;
	}

	let rooms = self
		.services
		.state_cache
		.rooms_joined(user_id)
		.chain(self.services.state_cache.rooms_invited(user_id));
	pin_mut!(rooms);
	while let Some(room_id) = rooms.next().await {
		let Ok(shortroomid) = self.services.short.get_shortroomid(room_id).await else {
			return true;
		};
		if self
			.changes
			.room_changed(room_id.as_bytes(), Some(shortroomid), since)
		{
			return true;
		}
		if self
			.services
			.typing
			.last_typing_update(room_id)
			.await
			.is_ok_and(|count| count > since)
		{
			return true;
		}
	}

	false
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
		}
	});
}

#[implement(Connection)]
#[tracing::instrument(level = "debug", skip_all)]
pub fn update_rooms_epilogue<'a, Rooms>(&mut self, window: Rooms)
where
	Rooms: Iterator<Item = &'a RoomId> + Send + 'a,
{
	window.for_each(|room_id| {
		let room = self.rooms.entry(room_id.into()).or_default();

		room.roomsince = self.next_batch;
	});
}

#[implement(Connection)]
#[tracing::instrument(level = "debug", skip_all)]
pub fn update_cache(&mut self, request: &Request) {
	Self::update_cache_lists(request, self);
	Self::update_cache_subscriptions(request, self);
	Self::update_cache_extensions(request, self);
}

#[implement(Connection)]
fn update_cache_lists(request: &Request, cached: &mut Self) {
	for (list_id, request_list) in &request.lists {
		cached
			.lists
			.entry(list_id.clone())
			.and_modify(|cached_list| {
				Self::update_cache_list(request_list, cached_list);
			})
			.or_insert_with(|| request_list.clone());
	}
}

#[implement(Connection)]
fn update_cache_list(request: &request::List, cached: &mut request::List) {
	list_or_sticky(&request.room_details.required_state, &mut cached.room_details.required_state);

	match (&request.filters, &mut cached.filters) {
		| (None, None) => {},
		| (None, Some(_cached)) => {},
		| (Some(request), None) => cached.filters = Some(request.clone()),
		| (Some(request), Some(cached)) => {
			some_or_sticky(request.is_dm.as_ref(), &mut cached.is_dm);
			some_or_sticky(request.is_encrypted.as_ref(), &mut cached.is_encrypted);
			some_or_sticky(request.is_invite.as_ref(), &mut cached.is_invite);
			list_or_sticky(&request.room_types, &mut cached.room_types);
			list_or_sticky(&request.not_room_types, &mut cached.not_room_types);
			list_or_sticky(&request.tags, &mut cached.not_tags);
			list_or_sticky(&request.spaces, &mut cached.spaces);
		},
	}
}

#[implement(Connection)]
fn update_cache_subscriptions(request: &Request, cached: &mut Self) {
	cached
		.subscriptions
		.extend(request.room_subscriptions.clone());
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
}

#[implement(Connection)]
fn update_cache_e2ee(request: &E2EE, cached: &mut E2EE) {
	some_or_sticky(request.enabled.as_ref(), &mut cached.enabled);
}

fn list_or_sticky<T: Clone>(target: &Vec<T>, cached: &mut Vec<T>) {
	if !target.is_empty() {
		cached.clone_from(target);
	}
}

fn some_or_sticky<T: Clone>(target: Option<&T>, cached: &mut Option<T>) {
	if let Some(target) = target {
		cached.replace(target.clone());
	}
}
