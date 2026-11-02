//! Durable transaction-response deduplication.
//!
//! Opaque response bytes are keyed by user, optional device, and transaction
//! identifier. Room operations add a domain tag, the room, and a scope (the
//! event type of a send, the target of a redaction) so independent scopes
//! cannot replay one another's responses.

use std::sync::Arc;

use ruma::{DeviceId, EventId, RoomId, TransactionId, UserId};
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

/// A client transaction against one room.
///
/// Paired with a [`RoomScope`], it keys the response a retry replays.
#[derive(Clone, Copy)]
pub struct RoomTxn<'a> {
	/// The user who sent the transaction.
	pub user_id: &'a UserId,

	/// The sending device, absent for a device-less sender.
	pub device_id: Option<&'a DeviceId>,

	/// The client's transaction identifier.
	pub txn_id: &'a TransactionId,

	/// The room the transaction acts on.
	pub room_id: &'a RoomId,
}

/// What a room transaction acts on within its room.
///
/// Each variant has its own domain tag, so a send and a redaction never share
/// a key.
#[derive(Clone, Copy)]
pub enum RoomScope<'a> {
	/// A send, scoped by its event type.
	Send(&'a str),

	/// A redaction, scoped by its target event.
	Redact(&'a EventId),
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
const ROOM_REDACT_TAG: &str = "room-redact";

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
/// Records a response under a scoped room transaction key.
///
/// A domain tag, the room, and the scope extend the legacy key so unrelated
/// room operations cannot alias. Writing the same complete key again replaces
/// its opaque response bytes.
pub fn add_room_txnid(&self, txn: RoomTxn<'_>, scope: RoomScope<'_>, data: &[u8]) {
	let key = room_txnid_key(txn, scope);

	self.db
		.userdevicetxnid_response
		.put_raw(key, data);
}

#[implement(Service)]
/// Looks up a response under a scoped room transaction key.
///
/// A domain tag, the room, and the scope extend the legacy key so unrelated
/// room operations cannot alias. A missing row remains a database not-found
/// error, and a hit returns the stored opaque bytes through a database handle.
pub async fn existing_room_txnid(
	&self,
	txn: RoomTxn<'_>,
	scope: RoomScope<'_>,
) -> Result<Handle<'_>> {
	let key = room_txnid_key(txn, scope);

	self.db.userdevicetxnid_response.qry(&key).await
}

fn txnid_key<'a>(
	user_id: &'a UserId,
	device_id: Option<&'a DeviceId>,
	txn_id: &'a TransactionId,
) -> Key<'a> {
	(user_id, device_id, txn_id)
}

fn room_txnid_key<'a>(txn: RoomTxn<'a>, scope: RoomScope<'a>) -> RoomKey<'a> {
	let (tag, scope) = match scope {
		| RoomScope::Send(event_type) => (ROOM_SEND_TAG, event_type),
		| RoomScope::Redact(event_id) => (ROOM_REDACT_TAG, event_id.as_str()),
	};

	(txn.user_id, txn.device_id, txn.txn_id, tag, txn.room_id, scope)
}

#[cfg(test)]
mod tests;
