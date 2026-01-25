use std::borrow::Borrow;

use futures::{FutureExt, Stream};
use ruma::EventId;

use super::AuthSet;
use crate::utils::stream::{IterStream, ReadyExt};

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
/// Outputs the event IDs that are not present in all the auth chains.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) fn auth_difference<'a, AuthSets, Id>(auth_sets: AuthSets) -> impl Stream<Item = Id>
where
	AuthSets: Stream<Item = AuthSet<Id>>,
	Id: Borrow<EventId> + Clone + Eq + Ord + Send + 'a,
{
	auth_sets
		.ready_fold_default(|ret: AuthSet<Id>, set| {
			ret.symmetric_difference(&set)
				.cloned()
				.collect::<AuthSet<Id>>()
		})
		.map(|set: AuthSet<Id>| set.into_iter().stream())
		.flatten_stream()
}
