use std::{borrow::Borrow, collections::BTreeMap};

use futures::{FutureExt, Stream};
use roaring::RoaringBitmap;
use ruma::EventId;
use tuwunel_core::utils::stream::{IterStream, ReadyExt};

use super::AuthSet;

struct RoaringState<Id: Ord> {
	id_to_index: BTreeMap<Id, u32>,
	index_to_id: Vec<Id>,
	union: RoaringBitmap,
	intersection: RoaringBitmap,
	first: bool,
}

impl<Id: Ord> Default for RoaringState<Id> {
	fn default() -> Self {
		Self {
			id_to_index: BTreeMap::new(),
			index_to_id: Vec::new(),
			union: RoaringBitmap::new(),
			intersection: RoaringBitmap::new(),
			first: true,
		}
	}
}

impl<Id: Ord + Clone> RoaringState<Id> {
	fn merge(mut self, set: AuthSet<Id>) -> Self {
		let mut bitmap = RoaringBitmap::new();
		for id in set {
			let idx = match self.id_to_index.get(&id) {
				| Some(&idx) => idx,
				| None => {
					let idx = u32::try_from(self.index_to_id.len()).expect("too many event IDs");
					self.id_to_index.insert(id.clone(), idx);
					self.index_to_id.push(id);
					idx
				},
			};
			bitmap.insert(idx);
		}

		if self.first {
			self.union.clone_from(&bitmap);
			self.intersection = bitmap;
			self.first = false;
		} else {
			self.union |= &bitmap;
			self.intersection &= bitmap;
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
/// Outputs the event IDs that are not present in all the auth chains.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) fn auth_difference<'a, AuthSets, Id>(auth_sets: AuthSets) -> impl Stream<Item = Id>
where
	AuthSets: Stream<Item = AuthSet<Id>>,
	Id: Borrow<EventId> + Clone + Eq + Ord + Send + 'a,
{
	auth_sets
		.ready_fold_default(RoaringState::<Id>::merge)
		.map(|state: RoaringState<Id>| {
			if state.first {
				Vec::new().into_iter().stream()
			} else {
				let diff = std::ops::Sub::sub(state.union, state.intersection);
				let result_ids: Vec<Id> = diff
					.into_iter()
					.map(move |idx| {
						let index = usize::try_from(idx).expect("idx fits in usize");
						state.index_to_id[index].clone()
					})
					.collect();
				result_ids.into_iter().stream()
			}
		})
		.flatten_stream()
}
