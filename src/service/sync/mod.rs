mod watch;

#[cfg(test)]
mod tests;

use std::{
	collections::{BTreeMap, btree_map::Entry},
	mem::replace,
	sync::Arc,
};

use futures::Stream;
use ruma::{
	DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, UserId,
	api::client::sync::sync_events::v5::{
		ConnId as ConnectionId, ListId, Request, request,
		request::{AccountData, E2EE, Profiles, Receipts, ToDevice, Typing},
	},
	profile::ProfileFieldName,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
use tuwunel_core::{
	Result, at, debug, err, implement, is_equal_to,
	smallvec::SmallVec,
	utils::{OptionExt, stream::TryIgnore},
};
use tuwunel_database::{Cbor, Deserialized, Map, serialize_val};

use crate::rooms::short::ShortStateHash;

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

/// What a sliding sync connection carries from one request to the next.
///
/// It is stored after every response, so a client resuming from a position
/// finds the lists, extension settings and per-room progress it left there.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Connection {
	pub globalsince: u64,
	pub next_batch: u64,
	pub lists: Lists,
	pub extensions: request::Extensions,
	pub subscriptions: Subscriptions,
	pub rooms: Rooms,

	/// Position of the response that last carried the syncing user's whole
	/// profile in the MSC4262 profiles extension.
	///
	/// Zero until a response has carried it, and again whenever the extension is
	/// switched off, so switching it back on sends the whole profile anew.
	#[serde(default)]
	pub own_profile_since: u64,

	/// Whether the connection asked for a profile field it did not have before.
	///
	/// It rides the whole profile above, so the rooms replay their slice of the
	/// change log until the client advances past the response carrying the
	/// widened field set, and it is forgotten with everything else the extension
	/// owes when the extension goes off.
	#[serde(default)]
	pub profiles_fields_widened: bool,
}

/// Delivery progress for one room on a Sliding Sync connection.
///
/// The cursor advances with complete ranges; configuration tracks room payloads.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Room {
	/// Position the room's content was last delivered up to.
	pub roomsince: u64,

	/// Hash of the room configuration last delivered.
	#[serde(default)]
	pub config_hash: u64,

	/// Fingerprints of the required-state selectors last delivered for this room.
	#[serde(default)]
	pub required_state: RequiredState,

	/// Snapshot the delivered required state was read against.
	///
	/// Zero when unknown, as for a room recorded by an older build or delivered
	/// without resolving every key its required state named: its deltas compare
	/// against the cursor and resend outliers until a delivery records one.
	#[serde(default)]
	pub shortstatehash: ShortStateHash,

	/// Fingerprints of the requested state keys whose events the last delivery
	/// found due but could not read.
	///
	/// The snapshot does not vouch for these keys, so the next delivery that
	/// compares state sends their events whatever the snapshot holds.
	#[serde(default, skip_serializing_if = "UnreadState::is_empty")]
	pub unread_state: UnreadState,

	/// The room's delivery progress before the last request advanced it, kept in
	/// memory for a retry of that request's position.
	#[serde(skip)]
	pub checkpoint: Option<Checkpoint>,
}

/// A room's delivery progress as a request resuming from `pos` found it.
///
/// A retry of that position restores it, so the retry repeats the delta a lost
/// response carried instead of resending the room's whole state. A restart
/// forgets it, and a retry then rewinds the room to that position and forgets
/// its delivered configuration.
#[derive(Clone, Debug)]
pub struct Checkpoint {
	/// The position the request resumed from.
	pos: u64,

	/// The room's cursor before the request advanced it.
	roomsince: u64,

	/// The delivered configuration the request replaced, boxed since most hold none.
	config: Option<Box<DeliveredConfig>>,
}

/// Fingerprints of delivered required-state selectors.
///
/// The inline capacity covers common room-list and open-room selections.
pub type RequiredState = SmallVec<[u64; 18]>;

/// A room configuration's hash and its required-state coverage.
///
/// The merged request configuration yields one per room. A delivered payload
/// records it with the snapshot its coverage was read against.
pub type RoomConfig = (u64, RequiredState);

/// Fingerprints of requested state keys whose due events a delivery could not read.
///
/// Only a read fault leaves any; two fit inline at the size of an empty `Vec`.
pub type UnreadState = SmallVec<[u64; 2]>;

/// A delivered room configuration's hash, its required-state coverage, the
/// snapshot that coverage was read against, and the keys whose events the
/// delivery could not read.
///
/// All four advance together only after a room payload is assembled.
pub type DeliveredConfig = (u64, RequiredState, ShortStateHash, UnreadState);

type Connections = TokioMutex<BTreeMap<ConnectionKey, ConnectionVal>>;
pub type ConnectionVal = Arc<TokioMutex<Connection>>;
pub type ConnectionKey = (OwnedUserId, Option<OwnedDeviceId>, Option<ConnectionId>);

pub type Subscriptions = BTreeMap<OwnedRoomId, request::ListConfig>;
pub type Lists = BTreeMap<ListId, request::List>;
pub type Rooms = BTreeMap<OwnedRoomId, Room>;

/// A completed room range, with the configuration its payload delivered, if any.
///
/// Without a configuration, completing the range advances only the room's cursor.
pub type RoomUpdate<'a> = (&'a RoomId, Option<DeliveredConfig>);

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

/// Returns the connection for a key, from the cache or else from storage.
///
/// An uncached connection for a removed device is returned fresh and left
/// uncached, and a device that cannot be read fails the load. Otherwise the
/// stored state, or a default when none is stored, is cached.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn load_or_init_connection(&self, key: &ConnectionKey) -> Result<ConnectionVal> {
	let mut cache = self.connections.lock().await;

	if let Some(conn) = cache.get(key) {
		return Ok(conn.clone());
	}

	// Left uncached, a removed device's connection can never be stored.
	if self.device_removed(key).await? {
		return Ok(ConnectionVal::default());
	}

	let conn: ConnectionVal = self
		.db
		.userdeviceconnid_conn
		.qry(key)
		.await
		.deserialized()
		.map(|Cbor(conn)| conn)
		.map(TokioMutex::new)
		.map(Arc::new)
		.unwrap_or_default();

	cache.insert(key.clone(), conn.clone());

	Ok(conn)
}

#[implement(Service)]
async fn device_removed(&self, (user_id, device_id, _): &ConnectionKey) -> Result<bool> {
	let exists = device_id
		.as_deref()
		.map_async(|device_id| {
			self.services
				.users
				.try_device_exists(user_id, device_id)
		})
		.await;

	confirmed_removed(exists)
}

/// Reads a device lookup as a removal only when it found no device.
///
/// No device to look up removes nothing, and a failed lookup stays an error.
fn confirmed_removed(exists: Option<Result<bool>>) -> Result<bool> {
	exists.transpose().map(is_equal_to!(Some(false)))
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

/// Stores a connection's state while the cache still holds that connection.
///
/// A connection dropped, or replaced by a newer load, while a request held it
/// is not stored again. The check and the write hold the one server-wide cache
/// lock, so no drop falls between them and every store briefly waits on it.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self, loaded, conn))]
pub async fn store_connection(
	&self,
	key: &ConnectionKey,
	loaded: &ConnectionVal,
	conn: &Connection,
) -> Result {
	// Encoded before the lock, so the wait on it stays short.
	let state = serialize_val(Cbor(conn))?;
	let cache = self.connections.lock().await;
	let cached = cache
		.get(key)
		.is_some_and(|current| Arc::ptr_eq(current, loaded));

	if !cached {
		debug!(?key, "Not persisting a dropped connection");
		return Ok(());
	}

	self.db.userdeviceconnid_conn.put_raw(key, state);

	debug!(
		since = %conn.globalsince,
		next_batch = %conn.next_batch,
		"Persisted connection state"
	);

	Ok(())
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

/// Rewind each room past a retried position to where that position left it.
///
/// A room checkpointed at the retried position is restored as the client last
/// saw it. Any other room past the position rewinds to it and forgets its
/// delivered configuration.
#[implement(Connection)]
#[tracing::instrument(level = "debug", skip(self))]
pub fn update_rooms_prologue(&mut self, retry_since: Option<u64>) {
	let Some(retry_since) = retry_since else {
		return;
	};

	self.rooms
		.values_mut()
		.filter(|room| room.roomsince > retry_since)
		.for_each(|room| room.rewind(retry_since));
}

#[implement(Room)]
fn rewind(&mut self, since: u64) {
	let (roomsince, config) = self.take_checkpoint(since).map_or_else(
		|| (since, Some(DeliveredConfig::default())),
		|Checkpoint { roomsince, config, .. }| (roomsince, config.map(|config| *config)),
	);

	self.roomsince = roomsince;
	if let Some(config) = config {
		self.replace_config(config);
	}
}

#[implement(Room)]
fn take_checkpoint(&mut self, pos: u64) -> Option<Checkpoint> {
	self.checkpoint
		.take()
		.filter(|checkpoint| checkpoint.pos == pos)
}

#[implement(Room)]
fn replace_config(
	&mut self,
	(config_hash, required_state, shortstatehash, unread_state): DeliveredConfig,
) -> DeliveredConfig {
	(
		replace(&mut self.config_hash, config_hash),
		replace(&mut self.required_state, required_state),
		replace(&mut self.shortstatehash, shortstatehash),
		replace(&mut self.unread_state, unread_state),
	)
}

/// Advance the per-room cursor for each complete bounded room range.
///
/// `roomsince` is the lower bound of every content query for its room. A failed
/// room keeps its cursor and retries the same range after a later wake. An
/// advanced room is checkpointed at `since`, the position the request resumed from.
#[implement(Connection)]
#[tracing::instrument(level = "debug", skip_all)]
pub fn update_rooms_epilogue<'a, Complete>(&mut self, since: u64, complete: Complete)
where
	Complete: Iterator<Item = RoomUpdate<'a>> + Send + 'a,
{
	let next_batch = self.next_batch;

	complete.for_each(|(room_id, config)| {
		self.rooms
			.entry(room_id.into())
			.or_default()
			.advance(since, next_batch, config);
	});
}

/// Advance the room for a request that resumed from `since`.
///
/// A checkpoint taken at `since` is kept; one from another position gives way to
/// the room as this request found it. The checkpoint keeps the first configuration
/// replaced at `since` that the delivery changed, so a retry restores what the
/// client last saw.
#[implement(Room)]
fn advance(&mut self, since: u64, next_batch: u64, config: Option<DeliveredConfig>) {
	let found = self.take_checkpoint(since).unwrap_or(Checkpoint {
		pos: since,
		roomsince: self.roomsince,
		config: None,
	});

	// An unchanged configuration needs no copy; the room still holds it.
	let replaced = config
		.map(|config| self.replace_config(config))
		.filter(|(config_hash, required_state, shortstatehash, unread_state)| {
			(*config_hash, required_state, *shortstatehash, unread_state) != self.delivered()
		});

	let checkpoint = Checkpoint {
		config: found.config.or_else(|| replaced.map(Box::new)),
		..found
	};

	self.roomsince = next_batch;
	self.checkpoint = Some(checkpoint);
}

#[implement(Room)]
#[inline]
fn delivered(&self) -> (u64, &RequiredState, ShortStateHash, &UnreadState) {
	(self.config_hash, &self.required_state, self.shortstatehash, &self.unread_state)
}

/// Records that this pass carried the syncing user's whole profile.
///
/// Called once the pass has assembled its extensions, so a pass that failed
/// leaves the profile owed to the next one.
#[implement(Connection)]
#[tracing::instrument(level = "debug", skip_all)]
pub fn update_profiles_epilogue(&mut self) {
	if self.own_profile_owed() {
		self.own_profile_since = self.next_batch;
	}
}

/// Whether a base is owed for a profile field the connection did not have
/// before.
///
/// MSC4262 asks for the widened fields of every user in the room subset, which
/// the room passes deliver by replaying their whole slice of the change log.
#[implement(Connection)]
#[inline]
#[must_use]
pub fn profiles_fields_owed(&self) -> bool {
	self.profiles_fields_widened && self.own_profile_owed()
}

/// Whether the syncing user's whole profile is owed to the profiles extension.
///
/// It is owed while the extension is on and the client has not acknowledged a
/// response carrying it: the connection is new, the extension was switched on
/// after the connection began, or the client is replaying from before the
/// response that carried it.
#[implement(Connection)]
#[inline]
#[must_use]
pub fn own_profile_owed(&self) -> bool {
	self.extensions.profiles.enabled.unwrap_or(false)
		&& (self.own_profile_since == 0 || self.own_profile_since > self.globalsince)
}

#[implement(Connection)]
#[tracing::instrument(level = "debug", skip_all)]
pub fn update_cache(&mut self, request: &Request) -> bool {
	let lists_changed = Self::update_cache_lists(request, self);
	let subscriptions_changed = Self::update_cache_subscriptions(request, self);

	let fields_widened = Self::update_cache_extensions(request, self);

	self.update_cache_profiles_owed(fields_widened);

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

/// Merges the request's extension settings into the connection.
///
/// Returns whether the MSC4262 field filter named a field the connection did
/// not have, which the profiles extension owes a base for.
#[implement(Connection)]
fn update_cache_extensions(request: &Request, cached: &mut Self) -> bool {
	let request = &request.extensions;
	let cached = &mut cached.extensions;

	Self::update_cache_account_data(&request.account_data, &mut cached.account_data);
	Self::update_cache_receipts(&request.receipts, &mut cached.receipts);
	Self::update_cache_typing(&request.typing, &mut cached.typing);
	Self::update_cache_to_device(&request.to_device, &mut cached.to_device);
	Self::update_cache_e2ee(&request.e2ee, &mut cached.e2ee);

	Self::update_cache_profiles(&request.profiles, &mut cached.profiles)
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

/// Merges the profiles extension settings into the connection.
///
/// Returns whether the request widened the field filter, which is read before
/// the merge overwrites the filter it compares against.
#[implement(Connection)]
fn update_cache_profiles(request: &Profiles, cached: &mut Profiles) -> bool {
	some_or_sticky(request.enabled.as_ref(), &mut cached.enabled);
	some_or_sticky(request.rooms.as_ref(), &mut cached.rooms);
	some_or_sticky(request.lists.as_ref(), &mut cached.lists);

	let Some(fields) = request.fields.as_ref() else {
		return false;
	};

	if let Some(cached) = cached.fields.as_mut() {
		normalize_profile_fields(cached);
	}

	let widened = fields_widened(Some(fields), cached.fields.as_deref());

	retain_profile_fields(&mut cached.fields, fields);

	widened
}

fn retain_profile_fields(
	cached: &mut Option<Vec<ProfileFieldName>>,
	fields: &[ProfileFieldName],
) {
	let retained = cached.get_or_insert_default();

	fields.clone_into(retained);
	normalize_profile_fields(retained);
}

fn normalize_profile_fields(fields: &mut Vec<ProfileFieldName>) {
	fields.sort_unstable();
	fields.dedup();
}

/// Whether the request names a profile field the connection did not ask for.
///
/// An absent cached filter already covers every field, and an absent request
/// keeps the cached one, so neither widens anything.
fn fields_widened(
	request: Option<&[ProfileFieldName]>,
	cached: Option<&[ProfileFieldName]>,
) -> bool {
	request
		.zip(cached)
		.is_some_and(|(request, cached)| {
			request
				.iter()
				.any(|name| cached.binary_search(name).is_err())
		})
}

#[implement(Connection)]
fn update_cache_e2ee(request: &E2EE, cached: &mut E2EE) {
	some_or_sticky(request.enabled.as_ref(), &mut cached.enabled);
}

#[implement(Connection)]
fn update_cache_profiles_owed(&mut self, fields_widened: bool) {
	if fields_widened || !self.extensions.profiles.enabled.unwrap_or(false) {
		self.own_profile_since = 0;
	}

	self.profiles_fields_widened =
		fields_widened || (self.profiles_fields_widened && self.own_profile_owed());
}

fn some_or_sticky<T: Clone>(target: Option<&T>, cached: &mut Option<T>) {
	if let Some(target) = target {
		cached.replace(target.clone());
	}
}
