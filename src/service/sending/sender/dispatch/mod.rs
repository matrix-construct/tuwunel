mod appservice;
mod federation;
mod push;

use futures::{FutureExt, future::BoxFuture};
use tuwunel_core::{Error, Result, implement};

use crate::sending::{Destination, SendingEvent, Service};

pub(super) type SendingError = (Destination, Error);
pub(super) type SendingResult = Result<Destination, SendingError>;
pub(super) type SendingFuture<'a> = BoxFuture<'a, SendingResult>;

#[implement(Service)]
pub(super) fn send_events(
	&self,
	dest: Destination,
	events: Vec<SendingEvent>,
) -> SendingFuture<'_> {
	debug_assert!(!events.is_empty(), "sending empty transaction");
	match dest {
		| Destination::Federation(server) => self
			.send_events_dest_federation(server, events)
			.boxed(), // heterogeneous SendingFutures
		| Destination::Appservice(id) => self
			.send_events_dest_appservice(id, events)
			.boxed(),
		| Destination::Push(user_id, pushkey) => self
			.send_events_dest_push(user_id, pushkey, events)
			.boxed(),
	}
}
