use std::{fmt::Debug, sync::Arc};

use futures::{Stream, StreamExt};
use ruma::{OwnedServerName, ServerName, UserId};
use tuwunel_core::{
	Error, Result, at, utils,
	utils::{ReadyExt, stream::TryIgnore},
};
use tuwunel_database::{Database, Deserialized, Map};

use super::{Destination, SendingEvent};

pub(super) type OutgoingItem = (Key, SendingEvent, Destination);
pub(super) type SendingItem = (Key, SendingEvent);
pub(super) type QueueItem = (Key, SendingEvent);
pub(super) type Key = Vec<u8>;

pub struct Data {
	servercurrentevent_data: Arc<Map>,
	servernameevent_data: Arc<Map>,
	servername_educount: Arc<Map>,
	pub(super) db: Arc<Database>,
	services: Arc<crate::services::OnceServices>,
}

impl Data {
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

	#[inline]
	pub(super) fn delete_active_request(&self, key: &[u8]) {
		self.servercurrentevent_data.remove(key);
	}

	pub(super) async fn delete_all_active_requests_for(&self, destination: &Destination) {
		let prefix = destination.get_prefix();
		self.servercurrentevent_data
			.raw_keys_prefix(&prefix)
			.ignore_err()
			.ready_for_each(|key| self.servercurrentevent_data.remove(key))
			.await;
	}

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

	pub(super) fn mark_as_active<'a, I>(&self, events: I)
	where
		I: Iterator<Item = &'a QueueItem>,
	{
		events
			.filter(|(key, _)| !key.is_empty())
			.for_each(|(key, val)| {
				let val = if let SendingEvent::Edu(val) = &val { &**val } else { &[] };

				self.servercurrentevent_data.insert(key, val);
				self.servernameevent_data.remove(key);
			});
	}

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

	pub(super) fn queue_requests<'a, I>(&self, requests: I) -> Vec<Vec<u8>>
	where
		I: Iterator<Item = (&'a SendingEvent, &'a Destination)> + Clone + Debug + Send,
	{
		let keys: Vec<_> = requests
			.clone()
			.map(|(event, dest)| {
				let mut key = dest.get_prefix();
				if let SendingEvent::Pdu(value) = event {
					key.extend(value.as_ref());
				} else {
					let count = self.services.globals.next_count();
					let count = count.to_be_bytes();
					key.extend(&count);
				}

				key
			})
			.collect();

		self.servernameevent_data.insert_batch(
			keys.iter()
				.map(Vec::as_slice)
				.zip(requests.map(at!(0)))
				.map(|(key, event)| {
					let value = if let SendingEvent::Edu(value) = &event {
						&**value
					} else {
						&[]
					};

					(key, value)
				}),
		);

		keys
	}

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

	/// The destination of every request waiting in the queue (not yet in
	/// flight), in key order: one item per request, so a destination's items
	/// are adjacent. Read-only.
	pub(super) fn queued_destinations(&self) -> impl Stream<Item = Destination> + Send + '_ {
		self.servernameevent_data
			.raw_keys()
			.ignore_err()
			.ready_filter_map(|key| {
				parse_servercurrentevent(key, &[])
					.ok()
					.map(|(dest, _)| dest)
			})
	}

	pub(super) fn set_latest_educount(&self, server_name: &ServerName, last_count: u64) {
		self.servername_educount
			.raw_put(server_name, last_count);
	}

	pub async fn get_latest_educount(&self, server_name: &ServerName) -> u64 {
		self.servername_educount
			.get(server_name)
			.await
			.deserialized()
			.unwrap_or(0)
	}
}

fn parse_servercurrentevent(key: &[u8], value: &[u8]) -> Result<(Destination, SendingEvent)> {
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

		(
			Destination::Appservice(server),
			if value.is_empty() {
				SendingEvent::Pdu(event.into())
			} else {
				SendingEvent::Edu(value.into())
			},
		)
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

		(
			Destination::Push(user_id.to_owned(), pushkey_string),
			if value.is_empty() {
				SendingEvent::Pdu(event.into())
			} else {
				// I'm pretty sure this should never be called
				SendingEvent::Edu(value.into())
			},
		)
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

#[cfg(test)]
mod tests {
	use super::{Destination, SendingEvent, parse_servercurrentevent};

	fn key(prefix: &[u8], suffix: &[u8]) -> Vec<u8> {
		let mut key = prefix.to_vec();
		key.extend_from_slice(suffix);
		key
	}

	/// The key a replica logged as its first local write: the appservice
	/// "ns-horizon", PDU id 0x02c6783c_1f0f5a15.
	#[test]
	fn observed_appservice_key() {
		let key: Vec<u8> = b"+ns-horizon\xff"
			.iter()
			.copied()
			.chain([0x00, 0x00, 0x00, 0x00, 0x02, 0xc6, 0x78, 0x3c])
			.chain([0x00, 0x00, 0x00, 0x00, 0x1f, 0x0f, 0x5a, 0x15])
			.collect();

		let (dest, event) = parse_servercurrentevent(&key, &[]).expect("parses");
		assert_eq!(dest, Destination::Appservice("ns-horizon".to_owned()));
		assert!(matches!(event, SendingEvent::Pdu(_)));
		assert_eq!(dest.get_prefix(), b"+ns-horizon\xff");
	}

	/// `queued_destinations` relies on every key of a destination parsing
	/// back to that destination, and on a destination's keys being adjacent
	/// in key order, so that `dedup` leaves each destination once.
	#[test]
	fn queued_destinations_dedup_in_key_order() {
		let a = Destination::Federation("a.example".try_into().expect("server name"));
		let ab = Destination::Federation("a.example.b".try_into().expect("server name"));
		let asv = Destination::Appservice("ns-horizon".to_owned());

		let mut keys = vec![
			key(&a.get_prefix(), &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1]),
			key(&ab.get_prefix(), &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2]),
			key(&a.get_prefix(), &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 3]),
			key(&asv.get_prefix(), &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 4]),
			key(&asv.get_prefix(), &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 5]),
		];
		keys.sort(); // RocksDB order

		let mut dests: Vec<Destination> = keys
			.iter()
			.map(|key| parse_servercurrentevent(key, &[]).expect("parses").0)
			.collect();
		dests.dedup();

		assert_eq!(dests.len(), 3, "{dests:?}");
		assert!(dests.contains(&a));
		assert!(dests.contains(&ab));
		assert!(dests.contains(&asv));
	}
}
