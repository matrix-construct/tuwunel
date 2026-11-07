use ruma::OwnedUserId;
use tokio::sync::oneshot::{Sender, channel};
use tuwunel_core::{Result, err, implement};

use super::{Destination, Service};

pub(super) struct Control {
	pub dest: Destination,
	pub drop: bool,
	pub done: Sender<Result>,
}

/// Drop durable and suppressed work for one pusher.
///
/// The sender serializes deletion against queue promotion. Already claimed
/// deliveries or suppressed flushes may continue and requeue older events;
/// new notifications can refill the queue.
#[implement(Service)]
pub async fn drop_push_queue(&self, user_id: OwnedUserId, pushkey: String) -> Result {
	self.control_push_queue(user_id, pushkey, true)
		.await
}

/// Request delivery of one pusher's durable queue through its sender.
///
/// Running deliveries and retry backoff remain in force. Suppressed events
/// retain their usual presence policy.
#[implement(Service)]
pub async fn retry_push_queue(&self, user_id: OwnedUserId, pushkey: String) -> Result {
	self.control_push_queue(user_id, pushkey, false)
		.await
}

#[implement(Service)]
async fn control_push_queue(&self, user_id: OwnedUserId, pushkey: String, drop: bool) -> Result {
	let dest = Destination::Push(user_id, pushkey);
	let shard = self.shard_id(&dest);
	let (done, response) = channel();

	self.controls[shard]
		.0
		.send(Control { dest, drop, done })
		.map_err(|e| err!("{e}"))?;

	response.await.map_err(|e| err!("{e}"))?
}
