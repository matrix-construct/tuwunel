use futures::{StreamExt, TryStreamExt};
use ruma::OwnedUserId;
use tuwunel_core::Result;
use tuwunel_service::sending::Destination;

use crate::admin_command;

#[admin_command]
pub(super) async fn list_queue(&self, user_id: OwnedUserId, pushkey: String) -> Result {
	let suppressed = self
		.services
		.pusher
		.suppressed_for_pushkey(&user_id, &pushkey);

	let dest = Destination::Push(user_id, pushkey);
	let sending = &self.services.sending.db;
	let active_requests = sending
		.active_requests_for(&dest)
		.map(|(_, event)| ("active", event));

	sending
		.queued_requests(&dest)
		.map(|(_, event)| ("queued", event))
		.chain(active_requests)
		.map(Ok)
		.try_for_each(async |(state, event)| write!(self, "{state}: {event:?}\n").await)
		.await?;

	for (room, pdu) in suppressed {
		write!(self, "suppressed: {room} {pdu:?}\n").await?;
	}

	self.write_str(
		"Queue listing complete. Active entries may be in flight; the queue can change during \
		 listing.",
	)
	.await
}
