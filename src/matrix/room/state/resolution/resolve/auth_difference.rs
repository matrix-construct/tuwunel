use std::{borrow::Borrow, collections::HashMap, hash::Hash};

use futures::{FutureExt, Stream};
use ruma::EventId;
use tuwunel_core::utils::stream::{IterStream, ReadyExt};

use super::AuthSet;
use crate::event_id::RandomState;

struct Counts<Id> {
	// Leading run of input chains holding each ID; a repeat within a chain is a no-op.
	streaks: HashMap<Id, usize, RandomState>,
	total: usize,
}

/// Returns auth events absent from at least one input chain.
///
/// The difference is the union of the chains minus their intersection.
/// Repeated event IDs count once per input chain, and output order is arbitrary.
#[tracing::instrument(level = "trace", skip_all)]
pub(super) fn auth_difference<'a, AuthSets, Id>(auth_sets: AuthSets) -> impl Stream<Item = Id>
where
	AuthSets: Stream<Item = AuthSet<Id>>,
	Id: Borrow<EventId> + Clone + Eq + Hash + Send + 'a,
{
	auth_sets
		.ready_fold_default(Counts::merge)
		.map(|Counts { streaks, total }: Counts<Id>| {
			streaks
				.into_iter()
				.filter_map(move |(id, streak)| streak.lt(&total).then_some(id))
				.stream()
		})
		.flatten_stream()
}

impl<Id> Default for Counts<Id> {
	fn default() -> Self { Self { streaks: HashMap::default(), total: 0 } }
}

impl<Id: Eq + Hash> Counts<Id> {
	fn merge(mut self, set: AuthSet<Id>) -> Self {
		self.total = self.total.saturating_add(1);
		for id in set {
			let streak = self.streaks.entry(id).or_default();

			if streak.saturating_add(1) == self.total {
				*streak = self.total;
			}
		}

		self
	}
}
