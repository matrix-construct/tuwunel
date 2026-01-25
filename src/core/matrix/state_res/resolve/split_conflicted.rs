use std::{collections::HashMap, hash::Hash};

use futures::{Stream, StreamExt};

use super::StateMap;
use crate::validated;

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
) -> (StateMap<Id>, StateMap<Vec<Id>>)
where
	Maps: Stream<Item = StateMap<Id>>,
	Id: Clone + Eq + Hash + Ord + Send + Sync + 'a,
{
	let state_maps: Vec<_> = state_maps.collect().await;

	let mut state_set_count = 0_usize;
	let mut occurrences = HashMap::<_, HashMap<_, usize>>::new();
	let state_maps = state_maps.iter().inspect(|_state| {
		state_set_count = validated!(state_set_count + 1);
	});

	for (k, v) in state_maps.into_iter().flat_map(|s| s.iter()) {
		let acc = occurrences
			.entry(k.clone())
			.or_default()
			.entry(v.clone())
			.or_default();

		*acc = acc.saturating_add(1);
	}

	let mut unconflicted_state_map = StateMap::new();
	let mut conflicted_state_set = StateMap::<Vec<Id>>::new();

	for (k, v) in occurrences {
		for (id, occurrence_count) in v {
			if occurrence_count == state_set_count {
				unconflicted_state_map.insert((k.0.clone(), k.1.clone()), id.clone());
			} else {
				conflicted_state_set
					.entry((k.0.clone(), k.1.clone()))
					.or_default()
					.push(id.clone());
			}
		}
	}

	(unconflicted_state_map, conflicted_state_set)
}
