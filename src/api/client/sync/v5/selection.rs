//! Numbered required-state selectors, so that merging the selections of many
//! lists handles each distinct selector once.

use std::collections::{BTreeMap, HashMap, HashSet};

use ruma::{api::client::sync::sync_events::v5::ListId, events::StateEventType};
use tuwunel_core::utils::math::expect_into;
use tuwunel_matrix::StateKey;
use tuwunel_service::sync::Connection;

pub(super) type Selector = (StateEventType, StateKey);

/// Numbers each distinct selector in the order it is first seen.
#[derive(Default)]
pub(super) struct Numbered<'a> {
	selectors: Vec<&'a Selector>,
	numbers: HashMap<&'a Selector, usize>,
}

/// A set of selector numbers.
///
/// Its last word is never zero, so equal sets are equal values.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(super) struct Bits(Vec<u64>);

/// Each list of a connection with its timeline limit and numbered selection,
/// prepared once for a pass.
pub(super) struct ListSelections<'a> {
	numbered: Numbered<'a>,
	lists: BTreeMap<&'a ListId, (usize, Bits)>,
}

impl<'a> Numbered<'a> {
	/// The numbers of a selection, numbering the selectors not seen before.
	pub(super) fn add(&mut self, selection: &'a [Selector]) -> Bits {
		let Self { selectors, numbers } = self;

		selection
			.iter()
			.fold(Bits::default(), |mut bits, selector| {
				bits.insert(*numbers.entry(selector).or_insert_with(|| {
					selectors.push(selector);
					selectors.len().saturating_sub(1)
				}));

				bits
			})
	}

	/// Whether a set holds the number of a selector.
	pub(super) fn contains(&self, bits: &Bits, selector: &Selector) -> bool {
		self.numbers
			.get(selector)
			.is_some_and(|&number| bits.contains(number))
	}

	fn selectors<'b>(&'b self, bits: &'b Bits) -> impl Iterator<Item = &'a Selector> + 'b {
		bits.iter()
			.filter_map(|number| self.selectors.get(number).copied())
	}
}

impl Bits {
	fn insert(&mut self, number: usize) {
		let word = number / 64;
		if self.0.len() <= word {
			self.0.resize(word.saturating_add(1), 0);
		}

		if let Some(bits) = self.0.get_mut(word) {
			*bits |= 1_u64.wrapping_shl(expect_into(number % 64));
		}
	}

	fn contains(&self, number: usize) -> bool {
		self.0
			.get(number / 64)
			.is_some_and(|bits| bits & 1_u64.wrapping_shl(expect_into(number % 64)) != 0)
	}

	pub(super) fn union(&mut self, other: &Self) {
		if self.0.len() < other.0.len() {
			self.0.resize(other.0.len(), 0);
		}

		self.0
			.iter_mut()
			.zip(&other.0)
			.for_each(|(bits, other)| *bits |= other);
	}

	pub(super) fn len(&self) -> usize {
		self.0
			.iter()
			.map(|bits| expect_into::<usize, _>(bits.count_ones()))
			.sum()
	}

	fn iter(&self) -> impl Iterator<Item = usize> + '_ {
		self.0
			.iter()
			.enumerate()
			.flat_map(|(word, &bits)| {
				(0..64_usize)
					.filter(move |&bit| bits & 1_u64.wrapping_shl(expect_into(bit)) != 0)
					.map(move |bit| word.saturating_mul(64).saturating_add(bit))
			})
	}
}

impl<'a> ListSelections<'a> {
	pub(super) fn new(conn: &'a Connection) -> Self {
		let mut numbered = Numbered::default();
		let lists = conn
			.lists
			.iter()
			.map(|(id, list)| {
				let config = &list.room_details;
				let limit = super::rooms::timeline_limit(config);

				(id, (limit, numbered.add(&config.required_state)))
			})
			.collect();

		Self { numbered, lists }
	}

	/// The number of distinct selectors across the lists.
	#[cfg(test)]
	pub(super) fn distinct(&self) -> usize { self.numbered.selectors.len() }

	/// The largest timeline limit and the union of the selections of the
	/// given lists, copying each distinct selector once.
	pub(super) fn merge<'b, I>(&self, list_ids: I) -> (usize, HashSet<Selector>)
	where
		I: IntoIterator<Item = &'b ListId>,
	{
		let (limit, bits) = list_ids
			.into_iter()
			.filter_map(|id| self.lists.get(id))
			.fold((0, Bits::default()), |(limit, mut bits), (list_limit, list_bits)| {
				bits.union(list_bits);

				(limit.max(*list_limit), bits)
			});

		(limit, self.numbered.selectors(&bits).cloned().collect())
	}
}
