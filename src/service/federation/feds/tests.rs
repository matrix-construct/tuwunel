#![cfg(test)]

use std::{
	collections::BTreeSet,
	convert::identity,
	future::pending,
	num::NonZeroUsize,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	},
	time::Duration,
};

use futures::{FutureExt, Stream, StreamExt, stream::iter};
use ruma::{OwnedServerName, server_name};
use tokio::{sync::Semaphore, time::sleep};
use tuwunel_core::{Result, err};

use super::{Fault, Opts, OutcomeExt, fanout_with, resolve_opts};

#[derive(Clone)]
enum Behavior {
	Good(Vec<u8>),
	Fail,
	Hang,
}

struct CancelGuard(Arc<AtomicBool>);

impl Drop for CancelGuard {
	fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
}

#[test]
fn opts_resolve_width_caps() {
	let width = |opts, config| {
		resolve_opts(opts, config, 15)
			.width
			.map(NonZeroUsize::get)
	};

	let with_width = |width| Opts {
		width: NonZeroUsize::new(width),
		..Opts::default()
	};

	assert_eq!(width(Opts::default(), 0), Some(32));
	assert_eq!(width(with_width(64), 0), Some(64));
	assert_eq!(width(with_width(64), 16), Some(16));
	assert_eq!(width(with_width(8), 16), Some(8));
	assert_eq!(width(Opts::default(), 64), Some(64));
}

#[tokio::test]
async fn survey_partitions_the_destination_set() {
	let outcomes: Vec<_> = fanout_with(scripted_pairs(), scripted_send, opts(5, 10))
		.collect()
		.await;

	let origins: BTreeSet<_> = outcomes
		.iter()
		.map(|outcome| outcome.origin.clone())
		.collect();

	assert_eq!(outcomes.len(), 5);
	assert_eq!(origins, destinations());
	assert!(
		outcomes
			.iter()
			.any(|outcome| matches!(outcome.result, Err(Fault::Elapsed)))
	);

	let grid = fanout_with(scripted_pairs(), scripted_send, opts(5, 10))
		.grid(identity)
		.await;

	let data_origins: BTreeSet<_> = grid
		.data
		.values()
		.flat_map(|origins| origins.iter().cloned())
		.collect();

	let fault_origins: BTreeSet<_> = grid.faults.keys().cloned().collect();
	let partition: BTreeSet<_> = data_origins
		.union(&grid.empty)
		.cloned()
		.chain(fault_origins.iter().cloned())
		.collect();

	assert!(data_origins.is_disjoint(&grid.empty));
	assert!(data_origins.is_disjoint(&fault_origins));
	assert!(grid.empty.is_disjoint(&fault_origins));
	assert_eq!(partition, destinations());
	assert_eq!(
		grid.data.get(&1),
		Some(&BTreeSet::from([
			server_name!("a.example").to_owned(),
			server_name!("b.example").to_owned(),
		]))
	);

	assert_eq!(grid.empty, BTreeSet::from([server_name!("empty.example").to_owned()]));
	assert!(matches!(grid.faults.get(server_name!("fail.example")), Some(Fault::Error(_))));
	assert!(matches!(grid.faults.get(server_name!("hang.example")), Some(Fault::Elapsed)));

	let tally = fanout_with(scripted_pairs(), scripted_send, opts(5, 10))
		.tally()
		.await;

	let tally_faults: BTreeSet<_> = tally.faults.keys().cloned().collect();

	assert!(tally.ok.is_disjoint(&tally_faults));
	assert_eq!(
		tally
			.ok
			.union(&tally_faults)
			.cloned()
			.collect::<BTreeSet<_>>(),
		destinations()
	);

	assert_eq!(tally_faults, fault_origins);
}

#[tokio::test]
async fn fanout_respects_the_concurrency_cap() {
	let in_flight = Arc::new(AtomicUsize::new(0));
	let maximum = Arc::new(AtomicUsize::new(0));
	let calls = Arc::new(AtomicUsize::new(0));
	let gate = Arc::new(Semaphore::new(0));
	let send = {
		let in_flight = in_flight.clone();
		let maximum = maximum.clone();
		let calls = calls.clone();
		let gate = gate.clone();

		move |_origin, ()| {
			let in_flight = in_flight.clone();
			let maximum = maximum.clone();
			let calls = calls.clone();
			let gate = gate.clone();

			async move {
				calls.fetch_add(1, Ordering::SeqCst);
				let current = in_flight
					.fetch_add(1, Ordering::SeqCst)
					.saturating_add(1);

				maximum.fetch_max(current, Ordering::SeqCst);
				let _permit = gate
					.acquire()
					.await
					.expect("test semaphore remains open");

				in_flight.fetch_sub(1, Ordering::SeqCst);

				Ok(())
			}
		}
	};

	let pairs = iter([
		(server_name!("a.example").to_owned(), ()),
		(server_name!("b.example").to_owned(), ()),
		(server_name!("c.example").to_owned(), ()),
		(server_name!("d.example").to_owned(), ()),
		(server_name!("e.example").to_owned(), ()),
		(server_name!("f.example").to_owned(), ()),
	]);

	let outcomes = fanout_with(pairs, send, opts(2, 100));
	futures::pin_mut!(outcomes);
	assert!(outcomes.next().now_or_never().is_none());
	assert_eq!(calls.load(Ordering::SeqCst), 2);
	assert_eq!(maximum.load(Ordering::SeqCst), 2);

	gate.add_permits(2);
	let outcomes: Vec<_> = outcomes.collect().await;

	assert_eq!(outcomes.len(), 6);
	assert_eq!(calls.load(Ordering::SeqCst), 6);
	assert_eq!(maximum.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn first_acceptable_survives_an_early_error_and_cancels_losers() {
	let dropped = Arc::new(AtomicBool::new(false));
	let send = {
		let dropped = dropped.clone();

		move |origin: OwnedServerName, ()| {
			let dropped = dropped.clone();

			async move {
				match origin.as_str() {
					| "error.example" => Err(err!(BadServerResponse("scripted failure"))),
					| "winner.example" => {
						sleep(Duration::from_millis(5)).await;
						Ok(7)
					},
					| _ => {
						let _guard = CancelGuard(dropped);
						pending::<Result<u8>>().await
					},
				}
			}
		}
	};

	let pairs = iter([
		(server_name!("error.example").to_owned(), ()),
		(server_name!("loser.example").to_owned(), ()),
		(server_name!("winner.example").to_owned(), ()),
	]);

	let winner = fanout_with(pairs, send, opts(3, 100))
		.first_acceptable(|response| *response == 7)
		.await;

	assert_eq!(
		winner.map(|(origin, _response)| origin),
		Some(server_name!("winner.example").to_owned())
	);

	assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn sweep_deadline_reports_undispatched_destinations() {
	let pairs = iter([
		(server_name!("slow.example").to_owned(), Behavior::Hang),
		(server_name!("tail-a.example").to_owned(), Behavior::Good(vec![1])),
		(server_name!("tail-b.example").to_owned(), Behavior::Good(vec![2])),
	]);

	let opts = Opts {
		width: NonZeroUsize::new(1),
		timeout: Some(Duration::from_secs(1)),
		sweep_deadline: Some(Duration::from_millis(100)),
		..Opts::default()
	};

	let outcomes: Vec<_> = fanout_with(pairs, scripted_send, opts)
		.collect()
		.await;

	let result = |origin| {
		outcomes
			.iter()
			.find(|outcome| outcome.origin == origin)
			.map(|outcome| &outcome.result)
	};

	assert!(matches!(result(server_name!("slow.example")), Some(Err(Fault::Elapsed))));
	assert!(matches!(result(server_name!("tail-a.example")), Some(Err(Fault::NotAttempted))));
	assert!(matches!(result(server_name!("tail-b.example")), Some(Err(Fault::NotAttempted))));
	assert_eq!(outcomes.len(), 3);
}

fn scripted_pairs() -> impl Stream<Item = (OwnedServerName, Behavior)> + Send {
	iter([
		(server_name!("a.example").to_owned(), Behavior::Good(vec![1])),
		(server_name!("b.example").to_owned(), Behavior::Good(vec![1])),
		(server_name!("empty.example").to_owned(), Behavior::Good(Vec::new())),
		(server_name!("fail.example").to_owned(), Behavior::Fail),
		(server_name!("hang.example").to_owned(), Behavior::Hang),
	])
}

async fn scripted_send(_origin: OwnedServerName, behavior: Behavior) -> Result<Vec<u8>> {
	match behavior {
		| Behavior::Good(data) => Ok(data),
		| Behavior::Fail => Err(err!(BadServerResponse("scripted failure"))),
		| Behavior::Hang => pending().await,
	}
}

fn destinations() -> BTreeSet<OwnedServerName> {
	[
		server_name!("a.example").to_owned(),
		server_name!("b.example").to_owned(),
		server_name!("empty.example").to_owned(),
		server_name!("fail.example").to_owned(),
		server_name!("hang.example").to_owned(),
	]
	.into()
}

fn opts(width: usize, timeout_ms: u64) -> Opts {
	Opts {
		width: NonZeroUsize::new(width),
		timeout: Some(Duration::from_millis(timeout_ms)),
		..Opts::default()
	}
}
