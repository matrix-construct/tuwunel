mod appservice;
mod federation;
mod push;

use futures::{FutureExt, future::BoxFuture};
use tuwunel_core::{Error, Result, implement};

use super::split::Split;
use crate::sending::{
	Destination, Service,
	data::{Keys, QueueItem},
};

pub(super) type SendingError = (Destination, Error);
pub(super) type SendingResult = Result<Destination, SendingError>;
pub(super) type SendingFuture<'a> = BoxFuture<'a, Completion>;

/// A finished transaction with the durable rows it carried.
///
/// Success acknowledges exactly these keys, leaving any other active row of
/// the destination in place. A transaction sending one room of a rejected
/// batch carries the split it advances. One whose rows all failed to load sent
/// nothing, which proves nothing about the peer, so it ends the split instead.
pub(super) struct Completion {
	pub(super) result: SendingResult,
	pub(super) keys: Keys,
	pub(super) split: Option<Split>,
}

#[implement(Service)]
pub(super) fn send_events(
	&self,
	dest: Destination,
	items: Vec<QueueItem>,
	split: Option<Split>,
) -> SendingFuture<'_> {
	debug_assert!(!items.is_empty(), "sending empty transaction");

	let (keys, events): (Keys, Vec<_>) = items.into_iter().unzip();
	let complete = |result, split| Completion { result, keys, split };

	match dest {
		| Destination::Federation(server) => self
			.send_events_dest_federation(server, events)
			.map(|(result, sent)| complete(result, split.filter(|_| sent)))
			.boxed(), // heterogeneous SendingFutures
		| Destination::Appservice(id) => self
			.send_events_dest_appservice(id, events)
			.map(|result| complete(result, split))
			.boxed(),
		| Destination::Push(user_id, pushkey) => self
			.send_events_dest_push(user_id, pushkey, events)
			.map(|result| complete(result, split))
			.boxed(),
	}
}
