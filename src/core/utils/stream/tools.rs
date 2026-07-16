//! StreamTools for futures::Stream

use std::{collections::HashMap, hash::Hash};

use arrayvec::ArrayVec;
use futures::{Stream, StreamExt};

use super::ReadyExt;
use crate::{expected, utils::rand::index};

/// StreamTools
///
/// This interface is not necessarily complete; feel free to add as-needed.
pub trait Tools<Item>
where
	Self: Stream<Item = Item> + Send + Sized,
	<Self as Stream>::Item: Send,
{
	fn counts(self) -> impl Future<Output = HashMap<Item, usize>> + Send
	where
		<Self as Stream>::Item: Eq + Hash;

	fn counts_by<K, F>(self, f: F) -> impl Future<Output = HashMap<K, usize>> + Send
	where
		F: Fn(Item) -> K + Send,
		K: Eq + Hash + Send;

	fn counts_by_with_cap<const CAP: usize, K, F>(
		self,
		f: F,
	) -> impl Future<Output = HashMap<K, usize>> + Send
	where
		F: Fn(Item) -> K + Send,
		K: Eq + Hash + Send;

	fn counts_with_cap<const CAP: usize>(
		self,
	) -> impl Future<Output = HashMap<Item, usize>> + Send
	where
		<Self as Stream>::Item: Eq + Hash;

	/// Reservoir-samples up to `N` items uniformly at random in a single
	/// pass, applying `f` only to the items retained. Items are drawn
	/// without replacement; the keys `f` derives may still repeat, so a key
	/// produced by twice as many items is twice as likely to appear.
	fn sample_by<const N: usize, K, F>(self, f: F) -> impl Future<Output = ArrayVec<K, N>> + Send
	where
		F: Fn(Item) -> K + Send,
		K: Send;

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
