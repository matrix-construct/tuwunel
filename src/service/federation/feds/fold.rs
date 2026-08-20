//! Terminal folds for federation fanout outcomes.
//!
//! Ordered maps and sets make diagnostic output deterministic without changing
//! completion-ordered request execution. These folds summarize unique room
//! destinations; callers using repeated origins consume raw outcomes instead.

use std::collections::{BTreeMap, BTreeSet};

use futures::{Stream, pin_mut};
use ruma::OwnedServerName;
use tuwunel_core::utils::stream::ReadyExt;

use super::{Fault, Outcome};

/// Servers contributing the same datum.
///
/// Ordered storage keeps diagnostic output stable across runs.
pub type Origins = BTreeSet<OwnedServerName>;

/// Failed destinations indexed by server.
///
/// Each unique destination retains its terminal failure reason.
pub type Faults = BTreeMap<OwnedServerName, Fault>;

/// Grid of response data and the servers reporting it.
///
/// Empty successful responses remain distinct from faults so all destinations
/// can be accounted for after the fold.
pub struct Grid<K> {
	/// Maps each response datum to the origins reporting it.
	///
	/// A response that contributes several data appears in every matching set.
	pub data: BTreeMap<K, Origins>,

	/// Contains origins whose successful response yielded no data.
	///
	/// These origins remain distinct from destinations that failed.
	pub empty: Origins,

	/// Contains origins that produced no successful response.
	///
	/// The mapped value preserves the terminal failure class.
	pub faults: Faults,
}

/// Successful and failed destinations from a completed fanout.
///
/// The successful origin set retains more information than a count while
/// keeping the broadcast verdict compact.
pub struct Tally {
	/// Contains origins that returned a successful response.
	///
	/// The set preserves origin identity rather than reducing it to a count.
	pub ok: Origins,

	/// Contains origins that produced no successful response.
	///
	/// The mapped value preserves the terminal failure class.
	pub faults: Faults,
}

/// Adds terminal aggregations to a federation outcome stream.
///
/// Each fold preserves per-origin failures while applying caller policy only
/// to successful typed responses.
pub trait OutcomeExt<R>
where
	Self: Stream<Item = Outcome<R>> + Send + Sized,
	R: Send,
{
	/// Merges successful responses and retains per-origin faults.
	///
	/// Successful values are folded in completion order from the supplied seed.
	fn merge<T, F>(self, init: T, merge: F) -> impl Future<Output = (T, Faults)> + Send
	where
		T: Send,
		F: Fn(T, R) -> T + Send;

	/// Inverts extracted response data into origin sets.
	///
	/// Successful responses with no extracted data enter the empty-origin set.
	fn grid<K, I, F>(self, extract: F) -> impl Future<Output = Grid<K>> + Send
	where
		K: Ord + Send,
		I: IntoIterator<Item = K>,
		F: Fn(R) -> I + Send;

	/// Partitions destinations into successful origins and faults.
	///
	/// The partition retains origin identity and each terminal failure reason.
	fn tally(self) -> impl Future<Output = Tally> + Send;

	/// Returns the first completed successful response accepted by a predicate.
	///
	/// Dropping the search future cancels any remaining stream work.
	fn first_acceptable<F>(
		self,
		accept: F,
	) -> impl Future<Output = Option<(OwnedServerName, R)>> + Send
	where
		F: Fn(&R) -> bool + Send;
}

impl<S, R> OutcomeExt<R> for S
where
	S: Stream<Item = Outcome<R>> + Send + Sized,
	R: Send,
{
	fn merge<T, F>(self, init: T, merge: F) -> impl Future<Output = (T, Faults)> + Send
	where
		T: Send,
		F: Fn(T, R) -> T + Send,
	{
		self.ready_fold((init, Faults::new()), move |(merged, mut faults), outcome| match outcome
			.result
		{
			| Ok(response) => (merge(merged, response), faults),
			| Err(fault) => {
				faults.insert(outcome.origin, fault);
				(merged, faults)
			},
		})
	}

	fn grid<K, I, F>(self, extract: F) -> impl Future<Output = Grid<K>> + Send
	where
		K: Ord + Send,
		I: IntoIterator<Item = K>,
		F: Fn(R) -> I + Send,
	{
		let grid = Grid {
			data: BTreeMap::new(),
			empty: Origins::new(),
			faults: Faults::new(),
		};

		self.ready_fold(grid, move |mut grid, outcome| {
			match outcome.result {
				| Ok(response) => {
					let mut data = extract(response).into_iter();

					if let Some(mut datum) = data.next() {
						for next in data {
							grid.data
								.entry(datum)
								.or_default()
								.insert(outcome.origin.clone());

							datum = next;
						}

						grid.data
							.entry(datum)
							.or_default()
							.insert(outcome.origin);
					} else {
						grid.empty.insert(outcome.origin);
					}
				},
				| Err(fault) => {
					grid.faults.insert(outcome.origin, fault);
				},
			}

			grid
		})
	}

	fn tally(self) -> impl Future<Output = Tally> + Send {
		let tally = Tally {
			ok: Origins::new(),
			faults: Faults::new(),
		};

		self.ready_fold(tally, |mut tally, outcome| {
			match outcome.result {
				| Ok(_response) => {
					tally.ok.insert(outcome.origin);
				},
				| Err(fault) => {
					tally.faults.insert(outcome.origin, fault);
				},
			}

			tally
		})
	}

	async fn first_acceptable<F>(self, accept: F) -> Option<(OwnedServerName, R)>
	where
		F: Fn(&R) -> bool + Send,
	{
		let outcomes = self;

		pin_mut!(outcomes);
		outcomes
			.ready_find_map(move |outcome| match outcome.result {
				| Ok(response) if accept(&response) => Some((outcome.origin, response)),
				| _ => None,
			})
			.await
	}
}
