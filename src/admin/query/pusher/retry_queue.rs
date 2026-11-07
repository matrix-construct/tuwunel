use ruma::OwnedUserId;
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn retry_queue(&self, user_id: OwnedUserId, pushkey: String) -> Result {
	self.services
		.sending
		.retry_push_queue(user_id, pushkey)
		.await?;

	self.write_str(
		"Sender notified. In-flight delivery, retry backoff, and presence suppression remain in \
		 force.",
	)
	.await
}
