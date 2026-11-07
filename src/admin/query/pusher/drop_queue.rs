use ruma::OwnedUserId;
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn drop_queue(&self, user_id: OwnedUserId, pushkey: String) -> Result {
	self.services
		.sending
		.drop_push_queue(user_id, pushkey)
		.await?;

	self.write_str(
		"Pending queue dropped. Already claimed batches and suppressed flushes may continue and \
		 requeue older events; new notifications can refill the queue.",
	)
	.await
}
