use std::{
	cell::Cell,
	cmp::Reverse,
	collections::{BTreeMap, HashMap},
	hint::black_box,
	sync::Mutex,
	time::Instant,
};

use ruma::{OwnedServerName, server_name};
use tokio::time::Instant as TokioInstant;

use super::{SendingFutures, TransactionStatus, WakeQueue};
use crate::sending::{
	Destination, Msg, SendingEvent, StalledDestinations,
	sender::{dispatch::SendingFuture, select::Selection},
};

type Entries = HashMap<OwnedServerName, Option<Instant>>;
type OrderedEntries = BTreeMap<OwnedServerName, Option<Instant>>;

const LOOKUPS: usize = 10_000;
const SCANS: usize = 100;

#[test]
fn recovery_bookkeeping_costs() {
	eprintln!(
		"sizes status={} selection={} prior_selection={} message={} future={} futures={} \
		 heap_entry={} stalled_field={}",
		size_of::<TransactionStatus>(),
		size_of::<Selection>(),
		size_of::<Option<Vec<SendingEvent>>>(),
		size_of::<Msg>(),
		size_of::<SendingFuture<'_>>(),
		size_of::<SendingFutures<'_>>(),
		size_of::<Reverse<(TokioInstant, Destination)>>(),
		size_of::<StalledDestinations>(),
	);

	for count in [1, 100, 1_800, 10_000] {
		let hashed = Mutex::new(entries(count).collect::<Entries>());
		let ordered = Mutex::new(entries(count).collect::<OrderedEntries>());
		let missing = server_name!("absent.example");
		let found = server_name!("server00000.example");
		let hash_miss = elapsed(LOOKUPS, || {
			black_box(
				hashed
					.lock()
					.expect("locked")
					.get(missing)
					.is_some(),
			);
		});

		let tree_miss = elapsed(LOOKUPS, || {
			black_box(
				ordered
					.lock()
					.expect("locked")
					.get(missing)
					.is_some(),
			);
		});

		let hash_hit = elapsed(LOOKUPS, || {
			black_box(
				hashed
					.lock()
					.expect("locked")
					.get(found)
					.is_some(),
			);
		});

		let tree_hit = elapsed(LOOKUPS, || {
			black_box(
				ordered
					.lock()
					.expect("locked")
					.get(found)
					.is_some(),
			);
		});

		eprintln!(
			"stalled entries={count} lookups={LOOKUPS} hash_miss={hash_miss}ns \
			 tree_miss={tree_miss}ns hash_hit={hash_hit}ns tree_hit={tree_hit}ns"
		);

		let deadline = TokioInstant::now();
		let wakes: WakeQueue = entries(count)
			.map(|(server, _)| Reverse((deadline, Destination::Federation(server))))
			.collect();

		let destination = Destination::Federation(missing.to_owned());
		let comparisons = Cell::new(0);
		let scan = elapsed(SCANS, || {
			let armed = wakes
				.iter()
				.inspect(|_| comparisons.set(comparisons.get() + 1))
				.any(|Reverse((_, dest))| dest == &destination);

			assert!(!black_box(armed));
		});

		assert_eq!(comparisons.get(), count * SCANS);
		eprintln!(
			"wake entries={count} scans={SCANS} comparisons={} elapsed={scan}ns",
			comparisons.get()
		);
	}
}

fn elapsed(count: usize, work: impl Fn()) -> u128 {
	let started = Instant::now();

	for _ in 0..count {
		work();
	}

	started.elapsed().as_nanos()
}

fn entries(count: usize) -> impl Iterator<Item = (OwnedServerName, Option<Instant>)> {
	(0..count).map(|index| {
		let name = format!("server{index:05}.example");
		let server: OwnedServerName = name.as_str().try_into().expect("server name");

		(server, None)
	})
}
