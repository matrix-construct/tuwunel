mod watch;

use std::{
	collections::{BTreeMap, BTreeSet},
	sync::{Arc, Mutex, Mutex as StdMutex},
};

use ruma::{
	OwnedDeviceId, OwnedRoomId, OwnedUserId,
	api::client::sync::sync_events::v5::{Request, request},
};
use tuwunel_core::{Result, implement, smallstr::SmallString};
use tuwunel_database::Map;

pub struct Service {
	db: Data,
	services: Arc<crate::services::OnceServices>,
	snake_connections: DbConnections<SnakeConnectionsKey, SnakeConnectionsVal>,
}

pub struct Data {
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
}

#[derive(Debug, Default)]
struct SnakeSyncCache {
	lists: BTreeMap<ListId, request::List>,
	subscriptions: RoomSubscriptions,
	known_rooms: KnownRooms,
	extensions: request::Extensions,
}

pub type KnownRooms = BTreeMap<ListId, BTreeMap<OwnedRoomId, u64>>;
pub type RoomSubscriptions = BTreeMap<OwnedRoomId, request::RoomSubscription>;
pub type SnakeConnectionsKey = (OwnedUserId, OwnedDeviceId, Option<ConnId>);
type SnakeConnectionsVal = Arc<Mutex<SnakeSyncCache>>;
type DbConnections<K, V> = Mutex<BTreeMap<K, V>>;
pub type ListId = SmallString<[u8; 16]>;
pub type ConnId = SmallString<[u8; 16]>;

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
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
			},
			services: args.services.clone(),
			snake_connections: StdMutex::new(BTreeMap::new()),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
pub fn update_snake_sync_request_with_cache(
	&self,
	snake_key: &SnakeConnectionsKey,
	request: &mut Request,
) -> KnownRooms {
	let mut cache = self.snake_connections.lock().expect("locked");
	let cached = Arc::clone(
		cache
			.entry(snake_key.clone())
			.or_insert_with(|| Arc::new(Mutex::new(SnakeSyncCache::default()))),
	);

	let cached = &mut cached.lock().expect("locked");
	drop(cache);

	//Request::try_from_http_request(req, path_args);
	for (list_id, list) in &mut request.lists {
		if let Some(cached_list) = cached.lists.get(list_id.as_str()) {
			list_or_sticky(
				&mut list.room_details.required_state,
				&cached_list.room_details.required_state,
			);

			//some_or_sticky(&mut list.include_heroes, cached_list.include_heroes);

			match (&mut list.filters, cached_list.filters.clone()) {
				| (Some(filters), Some(cached_filters)) => {
					some_or_sticky(&mut filters.is_invite, cached_filters.is_invite);
					// TODO (morguldir): Find out how a client can unset this, probably need
					// to change into an option inside ruma
					list_or_sticky(&mut filters.not_room_types, &cached_filters.not_room_types);
				},
				| (_, Some(cached_filters)) => list.filters = Some(cached_filters),
				| (Some(list_filters), _) => list.filters = Some(list_filters.clone()),
				| (..) => {},
			}
		}

		cached
			.lists
			.insert(list_id.as_str().into(), list.clone());
	}

	cached
		.subscriptions
		.extend(request.room_subscriptions.clone());

	request
		.room_subscriptions
		.extend(cached.subscriptions.clone());

	request.extensions.e2ee.enabled = request
		.extensions
		.e2ee
		.enabled
		.or(cached.extensions.e2ee.enabled);

	request.extensions.to_device.enabled = request
		.extensions
		.to_device
		.enabled
		.or(cached.extensions.to_device.enabled);

	request.extensions.account_data.enabled = request
		.extensions
		.account_data
		.enabled
		.or(cached.extensions.account_data.enabled);
	request.extensions.account_data.lists = request
		.extensions
		.account_data
		.lists
		.clone()
		.or_else(|| cached.extensions.account_data.lists.clone());
	request.extensions.account_data.rooms = request
		.extensions
		.account_data
		.rooms
		.clone()
		.or_else(|| cached.extensions.account_data.rooms.clone());

	{
		let (request, cached) = (&mut request.extensions.typing, &cached.extensions.typing);
		some_or_sticky(&mut request.enabled, cached.enabled);
		some_or_sticky(&mut request.rooms, cached.rooms.clone());
		some_or_sticky(&mut request.lists, cached.lists.clone());
	};
	{
		let (request, cached) = (&mut request.extensions.receipts, &cached.extensions.receipts);
		some_or_sticky(&mut request.enabled, cached.enabled);
		some_or_sticky(&mut request.rooms, cached.rooms.clone());
		some_or_sticky(&mut request.lists, cached.lists.clone());
	};

	cached.extensions = request.extensions.clone();
	cached.known_rooms.clone()
}

#[implement(Service)]
pub fn update_snake_sync_known_rooms(
	&self,
	key: &SnakeConnectionsKey,
	list_id: ListId,
	new_cached_rooms: BTreeSet<OwnedRoomId>,
	globalsince: u64,
) {
	assert!(key.2.is_some(), "Some(conn_id) required for this call");

	let mut cache = self.snake_connections.lock().expect("locked");
	let cached = Arc::clone(
		cache
			.entry(key.clone())
			.or_insert_with(|| Arc::new(Mutex::new(SnakeSyncCache::default()))),
	);

	let cached = &mut cached.lock().expect("locked");
	drop(cache);

	for (room_id, lastsince) in cached
		.known_rooms
		.entry(list_id.clone())
		.or_default()
		.iter_mut()
	{
		if !new_cached_rooms.contains(room_id) {
			*lastsince = 0;
		}
	}

	let list = cached.known_rooms.entry(list_id).or_default();
	for room_id in new_cached_rooms {
		list.insert(room_id, globalsince);
	}
}

#[implement(Service)]
pub fn update_snake_sync_subscriptions(
	&self,
	key: &SnakeConnectionsKey,
	subscriptions: RoomSubscriptions,
) {
	let mut cache = self.snake_connections.lock().expect("locked");
	let cached = Arc::clone(
		cache
			.entry(key.clone())
			.or_insert_with(|| Arc::new(Mutex::new(SnakeSyncCache::default()))),
	);

	let cached = &mut cached.lock().expect("locked");
	drop(cache);

	cached.subscriptions = subscriptions;
}

#[implement(Service)]
pub fn forget_snake_sync_connection(&self, key: &SnakeConnectionsKey) {
	self.snake_connections
		.lock()
		.expect("locked")
		.remove(key);
}

#[implement(Service)]
pub fn snake_connection_cached(&self, key: &SnakeConnectionsKey) -> bool {
	self.snake_connections
		.lock()
		.expect("locked")
		.contains_key(key)
}

#[inline]
pub fn into_snake_key<U, D, C>(
	user_id: U,
	device_id: D,
	conn_id: Option<C>,
) -> SnakeConnectionsKey
where
	U: Into<OwnedUserId>,
	D: Into<OwnedDeviceId>,
	C: Into<ConnId>,
{
	(user_id.into(), device_id.into(), conn_id.map(Into::into))
}

/// load params from cache if body doesn't contain it, as long as it's allowed
/// in some cases we may need to allow an empty list as an actual value
fn list_or_sticky<T: Clone>(target: &mut Vec<T>, cached: &Vec<T>) {
	if target.is_empty() {
		target.clone_from(cached);
	}
}

fn some_or_sticky<T>(target: &mut Option<T>, cached: Option<T>) {
	if target.is_none() {
		*target = cached;
	}
}
