use std::sync::Arc;

use ruma::{DeviceId, TransactionId, UserId};
use tuwunel_core::{Result, implement};
use tuwunel_database::{Handle, Map};

pub struct Service {
	db: Data,
}

struct Data {
	userdevicetxnid_response: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				userdevicetxnid_response: args.db["userdevicetxnid_response"].clone(),
			},
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

#[implement(Service)]
pub fn add_txnid(
	&self,
	user_id: &UserId,
	device_id: Option<&DeviceId>,
	txn_id: &TransactionId,
	data: &[u8],
) {
	let mut key = user_id.as_bytes().to_vec();
	key.push(0xFF);
	key.extend_from_slice(
		device_id
			.map(DeviceId::as_bytes)
			.unwrap_or_default(),
	);
	key.push(0xFF);
	key.extend_from_slice(txn_id.as_bytes());

	self.db
		.userdevicetxnid_response
		.insert(&key, data);
}

// If there's no entry, this is a new transaction
#[implement(Service)]
pub async fn existing_txnid(
	&self,
	user_id: &UserId,
	device_id: Option<&DeviceId>,
	txn_id: &TransactionId,
) -> Result<Handle<'_>> {
	let key = (user_id, device_id, txn_id);
	self.db.userdevicetxnid_response.qry(&key).await
}
