#[cfg(test)]
mod tests;

use std::{
	fmt::Debug,
	iter::once,
	ops::{
		Bound,
		Bound::{Excluded, Included, Unbounded},
	},
	pin::pin,
	sync::Arc,
	time::Duration,
	vec,
};

use futures::{
	Stream, StreamExt,
	stream::{iter, unfold},
};
use ruma::{OwnedServerName, ServerName, UserId};
use tuwunel_core::{
	Error, Result, at, implement,
	matrix::ShortRoomId,
	utils,
	utils::{
		IterStream, ReadyExt,
		bytes::prefix_successor,
		str_from_bytes,
		stream::{TryIgnore, WidebandExt},
		time::now_secs,
	},
};
use tuwunel_database::{Database, Deserialized, Interfix, Map, Txn};

use super::{
	Destination, EduBuf, SendingEvent, TAG_BADGE_REFRESH, TAG_DEVICE_LIST_CHANGED, TAG_TO_DEVICE,
};

pub(super) type OutgoingItem = (Key, SendingEvent, Destination);
pub(super) type SendingItem = (Key, SendingEvent);

/// A queued event paired with its row key.
///
/// An empty key marks a synthetic wake that has no durable row.
pub(super) type QueueItem = (Key, SendingEvent);
pub(super) type Key = Vec<u8>;
pub(super) type Keys = Vec<Key>;

const PARK_BASE: Duration = Duration::from_hours(1);
const PARK_LIMIT: Duration = Duration::from_hours(24);

/// A room held back from a server that keeps rejecting it.
///
/// The server's queued rows for the room are skipped until `until` passes,
/// when the room is retried alone; a delivery ends the park.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Park {
	/// The room's short id.
	pub room: ShortRoomId,

	/// When the room may be sent again, in seconds since the epoch.
	pub until: u64,

	/// Consecutive parks without a delivery, each doubling the last.
	pub count: u64,
}

type ParkRow<'a> = ((&'a ServerName, ShortRoomId), (u64, u64));

/// The sending service's column families.
///
/// Queued rows wait in `servernameevent_data` until a transaction claims them
/// into `servercurrentevent_data`; `servername_educount` is the per-server EDU
/// watermark, and `servershortroomid_park` holds rooms a server keeps
/// rejecting.
pub struct Data {
	servercurrentevent_data: Arc<Map>,
	servernameevent_data: Arc<Map>,
	servername_educount: Arc<Map>,
	servershortroomid_park: Arc<Map>,
	pub(super) db: Arc<Database>,
	services: Arc<crate::services::OnceServices>,
}

#[implement(Data)]
pub(super) fn new(args: &crate::Args<'_>) -> Self {
	let db = &args.db;

	Self {
		servercurrentevent_data: db["servercurrentevent_data"].clone(),
		servernameevent_data: db["servernameevent_data"].clone(),
		servername_educount: db["servername_educount"].clone(),
		servershortroomid_park: db["servershortroomid_park"].clone(),
		db: args.db.clone(),
		services: args.services.clone(),
	}
}

#[implement(Data)]
#[inline]
pub(super) fn delete_active_request(&self, key: &[u8]) {
	self.servercurrentevent_data.remove(key);
}

/// Acknowledges the active rows one transaction carried.
///
/// Empty keys mark synthetic events, which have no row.
#[implement(Data)]
pub(super) fn delete_active_requests<'a, I>(&self, keys: I)
where
	I: IntoIterator<Item = &'a Key>,
{
	keys.into_iter()
		.filter(|key| !key.is_empty())
		.fold(self.db.txn(), |mut txn, key| {
			txn.del_raw(&self.servercurrentevent_data, key);
			txn
		})
		.execute();
}

#[implement(Data)]
pub(super) async fn delete_all_requests_for(&self, destination: &Destination) {
	let prefix = destination.get_prefix();

	self.servercurrentevent_data
		.raw_keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key| self.servercurrentevent_data.remove(key))
		.await;

	self.servernameevent_data
		.raw_keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key| self.servernameevent_data.remove(key))
		.await;
}

#[implement(Data)]
pub(super) fn mark_as_active<'a, I>(&self, events: I)
where
	I: Iterator<Item = &'a QueueItem>,
{
	events
		.filter(|(key, _)| !key.is_empty())
		.fold(self.db.txn(), |mut txn, (key, val)| {
			txn.insert_raw(&self.servercurrentevent_data, key, val.value_bytes());
			txn.del_raw(&self.servernameevent_data, key);
			txn
		})
		.execute();
}

/// Write composed EDUs straight into the active set, keyed by fresh counts.
///
/// Unlike `mark_as_active` there is no queue row to delete. Yields the new
/// keys in `edus` order for the transaction to acknowledge.
#[implement(Data)]
pub(super) fn persist_active_edus(
	&self,
	server: &ServerName,
	edus: &[EduBuf],
) -> vec::IntoIter<Key> {
	let dest = Destination::Federation(server.to_owned());

	// The permits retire their counts only once the rows have landed.
	let permits: Vec<_> = edus
		.iter()
		.map(|_| self.services.globals.next_count())
		.collect();

	let keys: Keys = permits
		.iter()
		.map(|permit| dest.count_key(**permit))
		.collect();

	let items = keys
		.iter()
		.map(Vec::as_slice)
		.zip(edus.iter().map(EduBuf::as_slice));

	Txn::insert(&self.servercurrentevent_data, items).execute();

	keys.into_iter()
}

/// Streams every active row across all destinations.
///
/// Rows are decoded as they are read; a row that fails to decode is a
/// corrupt database and panics.
#[implement(Data)]
#[inline]
pub fn active_requests(&self) -> impl Stream<Item = OutgoingItem> + Send + '_ {
	self.servercurrentevent_data
		.raw_stream()
		.ignore_err()
		.map(|(key, val)| {
			let (dest, event) =
				parse_servercurrentevent(key, val).expect("invalid servercurrentevent");

			(key.to_vec(), event, dest)
		})
}

/// Streams the active rows of one destination.
///
/// Rows are decoded as they are read; a row that fails to decode is a
/// corrupt database and panics.
#[implement(Data)]
#[inline]
pub fn active_requests_for(
	&self,
	destination: &Destination,
) -> impl Stream<Item = SendingItem> + Send + '_ + use<'_> {
	let prefix = destination.get_prefix();

	self.servercurrentevent_data
		.raw_stream_from(&prefix)
		.ignore_err()
		.ready_take_while(move |(key, _)| key.starts_with(&prefix))
		.map(queue_item)
}

#[implement(Data)]
pub(super) fn queue_requests<'a, I>(&self, requests: I) -> Keys
where
	I: Iterator<Item = (&'a SendingEvent, &'a Destination)> + Clone + Debug + Send,
{
	// The permits retire their counts only once the rows have landed.
	let (keys, _permits): (Keys, Vec<_>) = requests
		.clone()
		.map(|(event, dest)| match event {
			| SendingEvent::Pdu(pdu_id) => (dest.event_key(pdu_id), None),
			| _ => {
				let permit = self.services.globals.next_count();

				(dest.count_key(*permit), Some(permit))
			},
		})
		.unzip();

	let items = keys
		.iter()
		.map(Vec::as_slice)
		.zip(requests.map(at!(0)))
		.map(|(key, event)| (key, event.value_bytes()));

	Txn::insert(&self.servernameevent_data, items).execute();

	keys
}

/// Yields only pending queue items.
///
/// Empty-key payload wakes always pass because they have no durable row. A
/// wake can outlive its row after a completed drain delivered the event.
#[implement(Data)]
pub(super) fn retain_queued<'a, I>(
	&'a self,
	events: I,
) -> impl Stream<Item = QueueItem> + Send + 'a
where
	I: IntoIterator<Item = QueueItem> + Send + 'a,
	I::IntoIter: Send,
{
	iter(events).wide_filter_map(async |item| {
		let key = &item.0;
		let pending = key.is_empty()
			|| self
				.servernameevent_data
				.exists(key)
				.await
				.is_ok();

		pending.then_some(item)
	})
}

/// Streams the queued rows of one destination.
///
/// Rows are decoded as they are read; a row that fails to decode is a
/// corrupt database and panics.
#[implement(Data)]
pub fn queued_requests(
	&self,
	destination: &Destination,
) -> impl Stream<Item = QueueItem> + Send + '_ + use<'_> {
	let prefix = destination.get_prefix();

	self.queued_range(&prefix, prefix_end(prefix.clone()))
}

/// Streams the queued PDU rows of one room for a destination.
///
/// An EDU row whose count equals the room's short id has exactly the room's
/// key prefix and is skipped.
#[implement(Data)]
pub(super) fn queued_room(
	&self,
	destination: &Destination,
	room: ShortRoomId,
) -> impl Stream<Item = QueueItem> + Send + '_ + use<'_> {
	let start = destination.count_key(room);
	let len = start.len();
	let end = prefix_end(start.clone());

	self.queued_range(&start, end)
		.ready_filter(move |(key, _)| key.len() > len)
}

/// Streams the queued rows of one destination, past the PDU rows of `skip`.
///
/// `skip` must be sorted. Each skipped room costs one seek rather than a visit
/// per row, and the EDU row sharing a skipped room's key bytes is still yielded.
#[implement(Data)]
pub(super) fn queued_except<'a>(
	&'a self,
	destination: &'a Destination,
	skip: &'a [ShortRoomId],
) -> impl Stream<Item = QueueItem> + Send + 'a {
	debug_assert!(skip.is_sorted(), "skipped rooms must be sorted");

	let resumes = skip
		.iter()
		.map(|room| destination.count_key(room.saturating_add(1)));

	let ends = skip
		.iter()
		.map(|&room| Included(destination.count_key(room)))
		.chain(once(prefix_end(destination.get_prefix())));

	once(destination.get_prefix())
		.chain(resumes)
		.zip(ends)
		.stream()
		.flat_map(|(start, end)| self.queued_range(&start, end))
}

#[implement(Data)]
fn queued_range(
	&self,
	start: &[u8],
	end: Bound<Key>,
) -> impl Stream<Item = QueueItem> + Send + '_ + use<'_> {
	self.servernameevent_data
		.raw_stream_from(start)
		.ignore_err()
		.ready_take_while(move |&(key, _)| match &end {
			| Included(end) => key <= end.as_slice(),
			| Excluded(end) => key < end.as_slice(),
			| Unbounded => true,
		})
		.map(queue_item)
}

/// Moves a failed transaction's PDU rows back to the queue under their own keys.
///
/// EDU rows have no room, so they stay active and ride the next transaction.
/// A park lands in the same batch as the rows it holds back.
#[implement(Data)]
pub(super) fn demote<'a, I>(&self, items: I, park: Option<(&ServerName, Park)>)
where
	I: IntoIterator<Item = &'a QueueItem>,
{
	let txn = items
		.into_iter()
		.filter(|(_, event)| matches!(event, SendingEvent::Pdu(_)))
		.fold(self.db.txn(), |mut txn, (key, _)| {
			txn.insert_raw(&self.servernameevent_data, key, []);
			txn.del_raw(&self.servercurrentevent_data, key);
			txn
		});

	park.into_iter()
		.fold(txn, |mut txn, (server, park)| {
			txn.put(&self.servershortroomid_park, (server, park.room), (park.until, park.count));
			txn
		})
		.execute();
}

/// Computes the park for a room that failed again.
///
/// Each consecutive park doubles the last hold, up to a day; the caller
/// writes it.
#[implement(Data)]
pub(super) async fn next_park(&self, server: &ServerName, room: ShortRoomId) -> Park {
	let count = self
		.servershortroomid_park
		.qry(&(server, room))
		.await
		.deserialized()
		.map_or(1, |(_, count): (u64, u64)| count.saturating_add(1));

	let doublings = u32::try_from(count.saturating_sub(1)).unwrap_or(u32::MAX);
	let hold = PARK_BASE
		.saturating_mul(2_u32.saturating_pow(doublings))
		.min(PARK_LIMIT);

	Park {
		room,
		until: now_secs().saturating_add(hold.as_secs()),
		count,
	}
}

#[implement(Data)]
pub(super) fn unpark(&self, server: &ServerName, room: ShortRoomId) {
	self.servershortroomid_park.del((server, room));
}

/// Streams a server's parked rooms, expired ones included.
///
/// Rooms come in short id order, so their ids form a sorted skip list.
#[implement(Data)]
pub(super) fn parks<'a>(
	&'a self,
	server: &'a ServerName,
) -> impl Stream<Item = Park> + Send + 'a {
	self.servershortroomid_park
		.stream_prefix(&(server, Interfix))
		.ignore_err()
		.map(park_row)
		.map(at!(1))
}

/// Streams every parked room with its server, expired ones included.
///
/// The server is owned, so an item may be kept across polls.
#[implement(Data)]
pub fn parked(&self) -> impl Stream<Item = (OwnedServerName, Park)> + Send + '_ {
	self.servershortroomid_park
		.stream()
		.ignore_err()
		.map(park_row)
		.map(|(server, park)| (server.to_owned(), park))
}

fn queue_item((key, val): (&[u8], &[u8])) -> QueueItem {
	let (_, event) = parse_servercurrentevent(key, val).expect("invalid servercurrentevent");

	(key.to_vec(), event)
}

fn prefix_end(prefix: Key) -> Bound<Key> { prefix_successor(prefix).map_or(Unbounded, Excluded) }

fn park_row(((server, room), (until, count)): ParkRow<'_>) -> (&ServerName, Park) {
	(server, Park { room, until, count })
}

/// Streams queued push destinations with a pending badge refresh.
///
/// Returned destinations are owned and may safely cross cursor advances.
#[implement(Data)]
pub(super) fn queued_badge_refresh_destinations(
	&self,
) -> impl Stream<Item = Destination> + Send + '_ {
	self.servernameevent_data
		.raw_stream_from(b"$")
		.ignore_err()
		.ready_take_while(|(key, _)| key.starts_with(b"$"))
		.ready_filter_map(|(key, val)| {
			val.eq(&[TAG_BADGE_REFRESH]).then(|| {
				parse_servercurrentevent(key, val)
					.expect("invalid servercurrentevent")
					.0
			})
		})
}

/// Streams distinct queued federation destinations belonging to this worker.
///
/// Each seek copies one key into the owned cursor and skips its complete
/// destination prefix. Sigils and other shards are skipped before ownership.
#[implement(Data)]
pub(super) fn queued_federation_destinations<'a, F>(
	&'a self,
	owns: F,
) -> impl Stream<Item = Result<Destination>> + Send + 'a
where
	F: Fn(&ServerName) -> bool + Copy + Send + 'a,
{
	queued_destinations(move |key| self.seek_queued_key(key), owns)
}

fn queued_destinations<S, F, C>(
	seek: S,
	owns: C,
) -> impl Stream<Item = Result<Destination>> + Send
where
	S: Fn(Key) -> F + Send,
	F: Future<Output = Result<Option<Key>>> + Send,
	C: Fn(&ServerName) -> bool + Copy + Send,
{
	unfold((Some(Key::new()), seek, owns), async move |(lower, seek, owns)| {
		let lower = lower?;
		let (item, next) = match seek(lower).await {
			| Ok(None) => return None,
			| Err(error) => (Err(error), None),
			| Ok(Some(key)) => queued_destination(key, owns),
		};

		Some((item, (next, seek, owns)))
	})
	.ready_filter_map(Result::transpose)
}

fn queued_destination(
	key: Key,
	owns: impl Fn(&ServerName) -> bool,
) -> (Result<Option<Destination>>, Option<Key>) {
	match key.first() {
		| Some(b'$') => return (Ok(None), Some(single_key(key, b'%'))),
		| Some(b'+') => return (Ok(None), Some(single_key(key, b','))),
		| _ => {},
	}

	let Some(end) = key.iter().position(|byte| *byte == u8::MAX) else {
		let error = Error::bad_database("Queued federation key has no destination delimiter");

		return (Err(error), Some(after_key(key)));
	};

	let prefix = truncate_key(key, end.saturating_add(1));
	let destination = str_from_bytes(&prefix[..end])
		.ok()
		.and_then(|server| <&ServerName>::try_from(server).ok())
		.ok_or_else(|| Error::bad_database("Invalid queued federation destination"))
		.map(|server| owns(server).then(|| Destination::Federation(server.to_owned())));

	(destination, prefix_successor(prefix))
}

fn single_key(mut key: Key, byte: u8) -> Key {
	key.clear();
	key.push(byte);
	key
}

fn after_key(mut key: Key) -> Key {
	key.push(0);
	key
}

fn truncate_key(mut key: Key, len: usize) -> Key {
	key.truncate(len);
	key
}

#[implement(Data)]
#[tracing::instrument(level = "trace", skip_all)]
async fn seek_queued_key(&self, mut key: Key) -> Result<Option<Key>> {
	// The cursor callback copies into the owned seek buffer before its drop.
	let found = pin!(
		self.servernameevent_data
			.raw_keys_from(&key)
			.map(|item| item.map(|bytes| replace_key(&mut key, bytes)))
	)
	.next()
	.await
	.transpose()?
	.is_some();

	Ok(found.then_some(key))
}

fn replace_key(key: &mut Key, bytes: &[u8]) {
	key.clear();
	key.extend_from_slice(bytes);
}

#[implement(Data)]
pub(super) fn set_latest_educount(&self, server_name: &ServerName, last_count: u64) {
	self.servername_educount
		.raw_put(server_name, last_count);
}

/// The count of the newest EDU shipped to a server.
///
/// A server never shipped to reads as zero, so its first window starts at the
/// beginning of the log.
#[implement(Data)]
pub async fn get_latest_educount(&self, server_name: &ServerName) -> u64 {
	self.servername_educount
		.get(server_name)
		.await
		.deserialized()
		.unwrap_or(0)
}

impl SendingEvent {
	/// Return bytes written verbatim as the queue row value.
	///
	/// PDUs keep their ID in the row key and flushes are not persisted. EDU
	/// variants own `[tag][count][body]`; a badge refresh owns only its tag.
	pub(super) fn value_bytes(&self) -> &[u8] {
		match self {
			| Self::Edu(bytes) | Self::ToDevice(bytes) | Self::DeviceListChanged(bytes) => bytes,
			| Self::BadgeRefresh => &[TAG_BADGE_REFRESH],
			| Self::Pdu(_) | Self::Flush => &[],
		}
	}
}

pub(super) fn parse_servercurrentevent(
	key: &[u8],
	value: &[u8],
) -> Result<(Destination, SendingEvent)> {
	// Appservices start with a plus
	Ok::<_, Error>(if key.starts_with(b"+") {
		let mut parts = key[1..].splitn(2, |&b| b == 0xFF);

		let server = parts
			.next()
			.expect("splitn always returns one element");
		let event = parts
			.next()
			.ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;

		let server = utils::string_from_bytes(server).map_err(|_| {
			Error::bad_database("Invalid server bytes in server_currenttransaction")
		})?;

		let decoded = match value {
			| [] => SendingEvent::Pdu(event.into()),
			| [TAG_TO_DEVICE, ..] => SendingEvent::ToDevice(value.into()),
			| [TAG_DEVICE_LIST_CHANGED, ..] => SendingEvent::DeviceListChanged(value.into()),
			| _ => SendingEvent::Edu(value.into()),
		};

		(Destination::Appservice(server), decoded)
	} else if key.starts_with(b"$") {
		let mut parts = key[1..].splitn(3, |&b| b == 0xFF);

		let user = parts
			.next()
			.expect("splitn always returns one element");

		let user_string = str_from_bytes(user)
			.map_err(|_| Error::bad_database("Invalid user string in servercurrentevent"))?;

		let user_id = UserId::parse(user_string)
			.map_err(|_| Error::bad_database("Invalid user id in servercurrentevent"))?;

		let pushkey = parts
			.next()
			.ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
		let pushkey_string = utils::string_from_bytes(pushkey)
			.map_err(|_| Error::bad_database("Invalid pushkey in servercurrentevent"))?;

		let event = parts
			.next()
			.ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;

		(Destination::Push(user_id, pushkey_string), match value {
			| [] => SendingEvent::Pdu(event.into()),
			| [tag] if *tag == TAG_BADGE_REFRESH => SendingEvent::BadgeRefresh,
			| _ => SendingEvent::Edu(value.into()),
		})
	} else {
		let mut parts = key.splitn(2, |&b| b == 0xFF);

		let server = parts
			.next()
			.expect("splitn always returns one element");
		let event = parts
			.next()
			.ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;

		let server = utils::string_from_bytes(server).map_err(|_| {
			Error::bad_database("Invalid server bytes in server_currenttransaction")
		})?;

		(
			Destination::Federation(OwnedServerName::parse(&server).map_err(|_| {
				Error::bad_database("Invalid server string in server_currenttransaction")
			})?),
			if value.is_empty() {
				SendingEvent::Pdu(event.into())
			} else {
				SendingEvent::Edu(value.into())
			},
		)
	})
}
