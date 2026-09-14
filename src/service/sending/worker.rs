use std::{
	hash::{BuildHasher, BuildHasherDefault, DefaultHasher},
	mem::take,
	sync::Arc,
};

use futures::FutureExt;
use tokio::task::{JoinError, JoinSet, unconstrained};
use tuwunel_core::{
	Result, debug, err, error, implement,
	utils::{available_parallelism, math::usize_from_u64_truncated},
};

use super::{Destination, Msg, Service};
use crate::{Args, Service as _};

/// Run the sender workers to completion.
///
/// One worker is spawned per channel and joined; a panic among them is
/// returned so the manager restarts the service. The suppressed-push flush
/// tasks are then aborted and joined so a panic among them is still reported.
#[implement(Service)]
pub(super) async fn run(self: Arc<Self>) -> Result {
	let senders =
		self.channels
			.iter()
			.enumerate()
			.fold(JoinSet::new(), |mut senders, (id, _)| {
				let worker = self.clone().sender(id);
				let worker = if self.unconstrained() {
					unconstrained(worker).left_future()
				} else {
					worker.right_future()
				};

				senders.spawn_on(worker, self.server.runtime());
				senders
			});

	let result = join_senders(senders).await;
	let mut flushes = take(&mut *self.flushes.lock().expect("locked"));

	flushes.abort_all();
	while let Some(result) = flushes.join_next().await {
		log_flush(result);
	}

	result
}

/// Join the sender workers, stopping the rest on the first failure or panic.
async fn join_senders(mut senders: JoinSet<Result>) -> Result {
	while let Some(ret) = senders.join_next_with_id().await {
		let (id, result) = match ret {
			| Ok((id, result)) => (id, result),
			| Err(error) => (error.id(), Err(error.into())),
		};

		if let Err(error) = result {
			error!(?id, ?error, "sender worker failed");
			senders.shutdown().await;
			return Err(error);
		}

		debug!(?id, "sender worker finished");
	}

	Ok(())
}

#[implement(Service)]
pub(super) fn close(&self) {
	self.flushes.lock().expect("locked").abort_all();

	for (sender, _) in &self.channels {
		sender.close();
	}
}

#[implement(Service)]
pub(super) fn dispatch(&self, msg: Msg) -> Result {
	let shard = self.shard_id(&msg.dest);
	let sender = &self
		.channels
		.get(shard)
		.expect("missing sender worker channels")
		.0;

	debug_assert!(!sender.is_full(), "channel full");
	debug_assert!(!sender.is_closed(), "channel closed");
	sender.send(msg).map_err(|e| err!("{e}"))
}

#[implement(Service)]
pub(super) fn shard_id(&self, dest: &Destination) -> usize {
	if self.channels.len() <= 1 {
		return 0;
	}

	let hash = BuildHasherDefault::<DefaultHasher>::default().hash_one(dest);

	usize_from_u64_truncated(hash)
		.overflowing_rem(self.channels.len())
		.0
}

pub(super) fn num_senders(args: &Args<'_>) -> usize {
	const MIN_SENDERS: usize = 1;
	let max_senders = args
		.server
		.metrics
		.num_workers()
		.min(available_parallelism());

	// The config default 0 clamps to one sender; multiple senders are experimental.
	args.server
		.config
		.sender_workers
		.clamp(MIN_SENDERS, max_senders)
}

#[implement(Service)]
pub(super) fn spawn_flush<F>(&self, flush: F)
where
	F: Future<Output = ()> + Send + 'static,
{
	let mut flushes = self.flushes.lock().expect("locked");

	// Shutdown drains the set at the end of `run`; a flush spawned after that
	// would never be joined. A restart re-enters `run` and drains again.
	if !self.server.is_running() {
		return;
	}

	reap_flushes(&mut flushes);
	flushes.spawn_on(flush, self.server.runtime());
}

fn reap_flushes(flushes: &mut JoinSet<()>) {
	while let Some(result) = flushes.try_join_next() {
		log_flush(result);
	}
}

// A flush that panicked is reported here or nowhere; a cancelled one is the
// shutdown path.
fn log_flush(result: Result<(), JoinError>) {
	if let Err(error) = result
		&& error.is_panic()
	{
		error!(?error, "Suppressed push flush panicked");
	}
}
