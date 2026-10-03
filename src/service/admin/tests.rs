use std::{sync::Arc, time::Duration};

use tokio::{sync::mpsc::channel, time::timeout};
use tuwunel_core::{Result, config::Figment, err};

use super::Queue;
use crate::{Service as _, test_utils::fixture};

const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn interrupt_before_first_poll_stops_the_worker() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let admin = &fixture.services.admin;

	admin.interrupt().await;

	timeout(EXIT_TIMEOUT, Arc::clone(admin).worker())
		.await
		.map_err(|_| err!("admin worker kept running after an earlier interrupt"))??;

	let closed = matches!(*admin.queue.read().expect("locked for reading"), Queue::Closed);

	assert!(closed, "the worker reopened an interrupted queue");

	Ok(())
}

#[test]
fn only_an_interrupt_keeps_a_worker_from_opening() {
	let open = |mut queue: Queue| (queue.open(), queue);
	let live = |queue: &Queue| {
		queue
			.sender()
			.is_some_and(|sender| !sender.is_closed())
	};

	let (receiver, first) = open(Queue::Pending);

	assert!(receiver.is_some() && live(&first), "a first worker must open the queue");

	let (receiver, restarted) = open(Queue::Open(channel(1).0));

	assert!(
		receiver.is_some() && live(&restarted),
		"a restarted worker must replace the dead queue"
	);

	let (receiver, interrupted) = open(Queue::Closed);

	assert!(
		receiver.is_none() && matches!(interrupted, Queue::Closed),
		"a worker must not reopen an interrupted queue"
	);
}
