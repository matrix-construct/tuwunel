use ruma::UserId;
use tuwunel_core::implement;

use super::PASSWORD_DISABLED;

#[implement(super::Service)]
pub(super) fn create_disabled(&self, user_id: &UserId, origin: &str) {
	let mut txn = self.services.db.txn();

	txn.insert_raw(&self.db.userid_origin, user_id, origin);
	txn.insert_raw(&self.db.userid_password, user_id, PASSWORD_DISABLED);
	txn.execute();
}

#[cfg(test)]
mod tests;
