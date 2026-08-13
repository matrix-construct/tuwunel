//! StreamTools for futures::Stream

use std::{collections::HashMap, hash::Hash};

use arrayvec::ArrayVec;
use futures::{Stream, StreamExt};

use super::ReadyExt;
use crate::{expected, utils::rand::index};

/// Adds aggregation, sampling, and folding operations to streams.
///
/// Counting adapters consume the source into hash maps, while folding avoids an
/// intermediate collection. Reservoir sampling retains a uniform subset of up
/// to a fixed size in one pass.
pub trait Tools<Item>
where
	Self: Stream<Item = Item> + Send + Sized,
	<Self as Stream>::Item: Send,
{
	/// Counts occurrences of each distinct stream item.
	///
	/// The entire stream is consumed into a hash map that starts at zero
	/// capacity and grows as needed. Equal items share one incrementing
	/// counter.
	///
	/// # Panics
	///
	/// Panics if an item's occurrence count overflows `usize`.
	fn counts(self) -> impl Future<Output = HashMap<Item, usize>> + Send
	where
		<Self as Stream>::Item: Eq + Hash;

	/// Counts occurrences of keys derived from stream items.
	///
	/// `f` is applied once to every item before counting. The result map starts
	/// at zero capacity and grows as needed.
	///
	/// # Panics
	///
	/// Panics if a derived key's occurrence count overflows `usize`.
	fn counts_by<K, F>(self, f: F) -> impl Future<Output = HashMap<K, usize>> + Send
	where
		F: Fn(Item) -> K + Send,
		K: Eq + Hash + Send;

	/// Counts derived keys into a map with initial capacity `CAP`.
	///
	/// `f` is applied once to every stream item. The map initially reserves
	/// space for at least `CAP` distinct keys and grows as needed.
	///
	/// # Panics
	///
	/// Panics if a derived key's occurrence count overflows `usize`.
	fn counts_by_with_cap<const CAP: usize, K, F>(
		self,
		f: F,
	) -> impl Future<Output = HashMap<K, usize>> + Send
	where
		F: Fn(Item) -> K + Send,
		K: Eq + Hash + Send;

	/// Counts items into a map with initial capacity `CAP`.
	///
	/// Equal items share one counter as the entire stream is consumed. The map
	/// initially reserves space for at least `CAP` distinct items and grows as
	/// needed.
	///
	/// # Panics
	///
	/// Panics if an item's occurrence count overflows `usize`.
	fn counts_with_cap<const CAP: usize>(
		self,
	) -> impl Future<Output = HashMap<Item, usize>> + Send
	where
		<Self as Stream>::Item: Eq + Hash;

	/// Reservoir-samples up to `N` items uniformly without replacement.
	///
	/// The stream is consumed in one pass, and `f` is applied only when an item
	/// enters the reservoir, including entries later replaced. Rejected items
	/// do not invoke it, and derived keys may repeat.
	///
	/// # Panics
	///
	/// Panics if the number of observed stream items overflows `usize`.
	fn sample_by<const N: usize, K, F>(self, f: F) -> impl Future<Output = ArrayVec<K, N>> + Send
	where
		F: Fn(Item) -> K + Send,
		K: Send;

	/// Folds the stream from `T::default()` with an asynchronous accumulator.
	///
	/// Each item is processed in source order after the previous fold future
	/// resolves. The final accumulator is returned when the stream ends.
	fn fold_default<T, F, Fut>(self, f: F) -> impl Future<Output = T> + Send
	where
		F: Fn(T, Item) -> Fut + Send,
		Fut: Future<Output = T> + Send,
		T: Default + Send;
}

impl<Item, S> Tools<Item> for S
where
	S: Stream<Item = Item> + Send + Sized,
	<Self as Stream>::Item: Send,
{
	#[inline]
	fn counts(self) -> impl Future<Output = HashMap<Item, usize>> + Send
	where
		<Self as Stream>::Item: Eq + Hash,
	{
		self.counts_with_cap::<0>()
	}

	#[inline]
	fn counts_by<K, F>(self, f: F) -> impl Future<Output = HashMap<K, usize>> + Send
	where
		F: Fn(Item) -> K + Send,
		K: Eq + Hash + Send,
	{
		self.counts_by_with_cap::<0, K, F>(f)
	}

	#[inline]
	fn counts_by_with_cap<const CAP: usize, K, F>(
		self,
		f: F,
	) -> impl Future<Output = HashMap<K, usize>> + Send
	where
		F: Fn(Item) -> K + Send,
		K: Eq + Hash + Send,
	{
		self.map(f).counts_with_cap::<CAP>()
	}

	#[inline]
	fn counts_with_cap<const CAP: usize>(
		self,
	) -> impl Future<Output = HashMap<Item, usize>> + Send
	where
		<Self as Stream>::Item: Eq + Hash,
	{
		self.ready_fold(HashMap::with_capacity(CAP), |mut counts, item| {
			let entry = counts.entry(item).or_default();
			let value = *entry;
			*entry = expected!(value + 1);
			counts
		})
	}

	#[inline]
	fn sample_by<const N: usize, K, F>(self, f: F) -> impl Future<Output = ArrayVec<K, N>> + Send
	where
		F: Fn(Item) -> K + Send,
		K: Send,
	{
		self.enumerate()
			.ready_fold(ArrayVec::<K, N>::new(), move |mut reservoir, (i, item)| {
				if reservoir.len() < N {
					reservoir.push(f(item));
				} else {
					let slot = index(expected!(i + 1));
					if slot < N {
						reservoir[slot] = f(item);
					}
				}

				reservoir
			})
	}

	#[inline]
	fn fold_default<T, F, Fut>(self, f: F) -> impl Future<Output = T> + Send
	where
		F: Fn(T, Item) -> Fut + Send,
		Fut: Future<Output = T> + Send,
		T: Default + Send,
	{
		self.fold(T::default(), f)
	}
}
