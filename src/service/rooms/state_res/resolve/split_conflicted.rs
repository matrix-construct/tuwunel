use std::{collections::HashMap, hash::Hash, iter::IntoIterator};

use futures::{Stream, StreamExt};
use tuwunel_core::validated;

use super::{ConflictMap, StateMap};

/// Split the unconflicted state map and the conflicted state set.
///
/// Definition in the specification:
///
/// If a given key _K_ is present in every _Si_ with the same value _V_ in each
/// state map, then the pair (_K_, _V_) belongs to the unconflicted state map.
/// Otherwise, _V_ belongs to the conflicted state set.
///
/// It means that, for a given (event type, state key) tuple, if all state maps
/// have the same event ID, it lands in the unconflicted state map, otherwise
/// the event IDs land in the conflicted state set.
///
/// ## Arguments
///
/// * `state_maps` - The incoming states to resolve. Each `StateMap` represents
///   a possible fork in the state of a room.
///
/// ## Returns
///
/// Returns an `(unconflicted_state, conflicted_states)` tuple.
#[tracing::instrument(name = "split", level = "debug", skip_all)]
pub(super) async fn split_conflicted_state<'a, Maps, Id>(
	state_maps: Maps,
) -> (StateMap<Id>, ConflictMap<Id>)
where
	Maps: Stream<Item = StateMap<Id>>,
	Id: Clone + Eq + Hash + Ord + Send + Sync + 'a,
{
	let state_maps: Vec<_> = state_maps.collect().await;

	let state_ids_est = state_maps.iter().flatten().count();

	let state_set_count = state_maps
		.iter()
		.fold(0_usize, |acc, _| validated!(acc + 1));

	let mut occurrences = HashMap::<_, HashMap<_, usize>>::with_capacity(state_ids_est);

	for (k, v) in state_maps
		.into_iter()
		.flat_map(IntoIterator::into_iter)
	{
		let acc = occurrences
			.entry(k.clone())
			.or_default()
			.entry(v.clone())
			.or_default();

		*acc = acc.saturating_add(1);
	}

	let mut unconflicted_state_map = StateMap::new();
	let mut conflicted_state_set = ConflictMap::new();

	for (k, v) in occurrences {
		for (id, occurrence_count) in v {
			if occurrence_count == state_set_count {
				unconflicted_state_map.insert((k.0.clone(), k.1.clone()), id.clone());
			} else {
				let conflicts = conflicted_state_set
					.entry((k.0.clone(), k.1.clone()))
					.or_default();

				debug_assert!(
					!conflicts.contains(&id),
					"Unexpected duplicate conflicted state event"
				);

				conflicts.push(id.clone());
			}
		}
	}

	(unconflicted_state_map, conflicted_state_set)
}
