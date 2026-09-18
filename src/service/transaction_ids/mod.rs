//! Durable transaction-response deduplication.
//!
//! Opaque response bytes are keyed by user, optional device, and transaction
//! identifier. Room-send operations add a domain tag, room, and event type so
//! independent send scopes cannot replay one another's responses.

use std::sync::Arc;

use ruma::{DeviceId, RoomId, TransactionId, UserId};
use tuwunel_core::{Result, implement};
use tuwunel_database::{Handle, Map};

/// Persistent transaction-response lookup service.
///
/// Responses remain stored across process restarts and have no service-level
/// expiry. Adding a response for an existing key replaces the stored bytes.
/// Lookup and insertion are separate operations; the service does not reserve a
/// transaction identifier against concurrent execution.
pub struct Service {
	db: Data,
}

struct Data {
	userdevicetxnid_response: Arc<Map>,
}

type Key<'a> = (&'a UserId, Option<&'a DeviceId>, &'a TransactionId);
type RoomKey<'a> = (
	&'a UserId,
	Option<&'a DeviceId>,
	&'a TransactionId,
	&'static str,
	&'a RoomId,
	&'a str,
);

const ROOM_SEND_TAG: &str = "room-send";

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				userdevicetxnid_response: args.db["userdevicetxnid_response"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
/// Records a response under the legacy transaction scope.
///
/// The key contains the user, optional device, and transaction identifier.
/// Writing the same key again replaces its opaque response bytes while
/// preserving the original non-room key layout.
pub fn add_txnid(
	&self,
	user_id: &UserId,
	device_id: Option<&DeviceId>,
	txn_id: &TransactionId,
	data: &[u8],
) {
	let key = txnid_key(user_id, device_id, txn_id);

	self.db
		.userdevicetxnid_response
		.put_raw(key, data);
}

#[implement(Service)]
/// Looks up a response under the legacy transaction scope.
///
/// The key contains the user, optional device, and transaction identifier.
/// A missing row remains a database not-found error, and a hit returns the
/// stored opaque bytes through a database handle.
pub async fn existing_txnid(
	&self,
	user_id: &UserId,
	device_id: Option<&DeviceId>,
	txn_id: &TransactionId,
) -> Result<Handle<'_>> {
	let key = txnid_key(user_id, device_id, txn_id);

	self.db.userdevicetxnid_response.qry(&key).await
}

#[implement(Service)]
/// Records a response under the scoped room-send transaction key.
///
/// A domain tag, room, and event type extend the legacy key so unrelated send
/// scopes cannot alias. Writing the same complete key again replaces its
/// opaque response bytes.
pub fn add_room_txnid(
	&self,
	user_id: &UserId,
	device_id: Option<&DeviceId>,
	txn_id: &TransactionId,
	room_id: &RoomId,
	event_type: &str,
	data: &[u8],
) {
	let key = room_txnid_key(user_id, device_id, txn_id, room_id, event_type);

	self.db
		.userdevicetxnid_response
		.put_raw(key, data);
}

#[implement(Service)]
/// Looks up a response under the scoped room-send transaction key.
///
/// A domain tag, room, and event type extend the legacy key so unrelated send
/// scopes cannot alias. A missing row remains a database not-found error, and
/// a hit returns the stored opaque bytes through a database handle.
pub async fn existing_room_txnid(
	&self,
	user_id: &UserId,
	device_id: Option<&DeviceId>,
	txn_id: &TransactionId,
	room_id: &RoomId,
	event_type: &str,
) -> Result<Handle<'_>> {
	let key = room_txnid_key(user_id, device_id, txn_id, room_id, event_type);

	self.db.userdevicetxnid_response.qry(&key).await
}

fn txnid_key<'a>(
	user_id: &'a UserId,
	device_id: Option<&'a DeviceId>,
	txn_id: &'a TransactionId,
) -> Key<'a> {
	(user_id, device_id, txn_id)
}

fn room_txnid_key<'a>(
	user_id: &'a UserId,
	device_id: Option<&'a DeviceId>,
	txn_id: &'a TransactionId,
	room_id: &'a RoomId,
	event_type: &'a str,
) -> RoomKey<'a> {
	(user_id, device_id, txn_id, ROOM_SEND_TAG, room_id, event_type)
}

#[cfg(test)]
mod tests;
