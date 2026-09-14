use std::{fmt::Debug, sync::Arc};

use futures::{Stream, StreamExt, stream::iter};
use ruma::{OwnedServerName, ServerName, UserId};
use tuwunel_core::{
	Error, Result, at, implement, utils,
	utils::{
		ReadyExt,
		stream::{TryIgnore, WidebandExt},
	},
};
use tuwunel_database::{Database, Deserialized, Map, Txn};

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

/// The sending service's column families.
///
/// Queued rows wait in `servernameevent_data` until a transaction claims them
/// into `servercurrentevent_data`; `servername_educount` is the per-server EDU
/// watermark.
pub struct Data {
	servercurrentevent_data: Arc<Map>,
	servernameevent_data: Arc<Map>,
	servername_educount: Arc<Map>,
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
		db: args.db.clone(),
		services: args.services.clone(),
	}
}

#[implement(Data)]
#[inline]
pub(super) fn delete_active_request(&self, key: &[u8]) {
	self.servercurrentevent_data.remove(key);
}

#[implement(Data)]
pub(super) async fn delete_all_active_requests_for(&self, destination: &Destination) {
	let prefix = destination.get_prefix();

	self.servercurrentevent_data
		.raw_keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key| self.servercurrentevent_data.remove(key))
		.await;
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
/// Unlike `mark_as_active` there is no queue row to delete.
#[implement(Data)]
pub(super) fn persist_active_edus(&self, server: &ServerName, edus: &[EduBuf]) {
	let dest = Destination::Federation(server.to_owned());

	// The permits retire their counts only once the rows have landed.
	let permits: Vec<_> = edus
		.iter()
		.map(|_| self.services.globals.next_count())
		.collect();

	let items = permits
		.iter()
		.zip(edus)
		.map(|(permit, edu)| (dest.count_key(**permit), edu.as_slice()));

	Txn::insert(&self.servercurrentevent_data, items).execute();
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
		.map(|(key, val)| {
			let (_, event) =
				parse_servercurrentevent(key, val).expect("invalid servercurrentevent");

			(key.to_vec(), event)
		})
}

#[implement(Data)]
pub(super) fn queue_requests<'a, I>(&self, requests: I) -> Vec<Key>
where
	I: Iterator<Item = (&'a SendingEvent, &'a Destination)> + Clone + Debug + Send,
{
	// The permits retire their counts only once the rows have landed.
	let (keys, _permits): (Vec<Key>, Vec<_>) = requests
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

	self.servernameevent_data
		.raw_stream_from(&prefix)
		.ignore_err()
		.ready_take_while(move |(key, _)| key.starts_with(&prefix))
		.map(|(key, val)| {
			let (_, event) =
				parse_servercurrentevent(key, val).expect("invalid servercurrentevent");

			(key.to_vec(), event)
		})
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
		let user_string = utils::str_from_bytes(user)
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
