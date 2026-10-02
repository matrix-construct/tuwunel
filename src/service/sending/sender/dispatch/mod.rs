mod appservice;
mod federation;
mod push;

use futures::{FutureExt, future::BoxFuture};
use tuwunel_core::{Error, Result, implement};

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
/// the destination in place.
pub(super) struct Completion {
	pub(super) result: SendingResult,
	pub(super) keys: Keys,
}

#[implement(Service)]
pub(super) fn send_events(&self, dest: Destination, items: Vec<QueueItem>) -> SendingFuture<'_> {
	debug_assert!(!items.is_empty(), "sending empty transaction");

	let (keys, events): (Keys, Vec<_>) = items.into_iter().unzip();
	let complete = |result| Completion { result, keys };

	match dest {
		| Destination::Federation(server) => self
			.send_events_dest_federation(server, events)
			.map(complete)
			.boxed(), // heterogeneous SendingFutures
		| Destination::Appservice(id) => self
			.send_events_dest_appservice(id, events)
			.map(complete)
			.boxed(),
		| Destination::Push(user_id, pushkey) => self
			.send_events_dest_push(user_id, pushkey, events)
			.map(complete)
			.boxed(),
	}
}
