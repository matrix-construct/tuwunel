use std::{borrow::Borrow, collections::HashMap, hash::Hash};

use futures::{FutureExt, Stream};
use ruma::EventId;
use tuwunel_core::{
	matrix::event_id::RandomState,
	utils::stream::{IterStream, ReadyExt},
};

use super::AuthSet;

struct Counts<Id> {
	by_id: HashMap<Id, usize, RandomState>,
	total: usize,
}

impl<Id> Default for Counts<Id> {
	fn default() -> Self { Self { by_id: HashMap::default(), total: 0 } }
}

impl<Id: Eq + Hash> Counts<Id> {
	fn merge(mut self, set: AuthSet<Id>) -> Self {
		self.total = self.total.saturating_add(1);
		for id in set {
			let count = self.by_id.entry(id).or_default();

			*count = count.saturating_add(1);
		}

		self
	}
}

/// Get the auth difference for the given auth chains.
///
/// Definition in the specification:
///
/// The auth difference is calculated by first calculating the full auth chain
/// for each state _Si_, that is the union of the auth chains for each event in
/// _Si_, and then taking every event that doesn’t appear in every auth chain.
/// If _Ci_ is the full auth chain of _Si_, then the auth difference is ∪_Ci_ −
/// ∩_Ci_.
///
/// ## Arguments
///
/// * `auth_chains` - The list of full recursive sets of `auth_events`. Inputs
///   must be sorted.
///
/// ## Returns
///
/// Outputs the event IDs that are not present in all the auth chains, in no
/// particular order.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) fn auth_difference<'a, AuthSets, Id>(auth_sets: AuthSets) -> impl Stream<Item = Id>
where
	AuthSets: Stream<Item = AuthSet<Id>>,
	Id: Borrow<EventId> + Clone + Eq + Hash + Send + 'a,
{
	auth_sets
		.ready_fold_default(Counts::<Id>::merge)
		.map(|Counts { by_id, total }: Counts<Id>| {
			by_id
				.into_iter()
				.filter_map(move |(id, count)| (count < total).then_some(id))
				.stream()
		})
		.flatten_stream()
}
