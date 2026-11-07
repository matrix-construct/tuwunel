use futures::StreamExt;
use tuwunel_core::implement;

use super::{DEQUEUE_LIMIT, SendingFutures, TransactionStatuses, WakeQueue};
use crate::sending::{Destination, Service, queue::Control};

#[implement(Service)]
pub(super) async fn handle_control<'a>(
	&'a self,
	Control { dest, drop, done }: Control,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	let result = if drop {
		let result = self.db.delete_all_requests_for(&dest).await;

		if let Destination::Push(user_id, pushkey) = &dest {
			self.services
				.pusher
				.clear_suppressed_pushkey(user_id, pushkey);
		}

		result
	} else {
		let events = self
			.db
			.queued_requests(&dest)
			.take(DEQUEUE_LIMIT)
			.collect()
			.await;

		self.select_events(&dest, events, statuses)
			.await
			.map(|selection| self.schedule_events(dest, selection, futures, statuses, wakes))
	};

	done.send(result).ok();
}
