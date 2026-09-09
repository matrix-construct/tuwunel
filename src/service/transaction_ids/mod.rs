use std::sync::Arc;

use ruma::{DeviceId, RoomId, TransactionId, UserId};
use tuwunel_core::{Result, implement};
use tuwunel_database::{Handle, Map};

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
/// This preserves the original non-room key layout for existing callers.
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
/// This preserves the original non-room key layout for existing callers.
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
/// Room and event type are included so unrelated send endpoints cannot alias.
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
/// Room and event type are included so unrelated send endpoints cannot alias.
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
