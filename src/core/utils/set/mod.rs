//! Set operations on iterators and streams.
//!
//! Sorted variants require their inputs to be ascending under a total order
//! and preserve multiplicity. The unsorted [`intersection`] runs in O(n*m)
//! and is provided for convenience when sorting is unavailable.

mod difference_sorted_stream2;
mod intersection;
mod intersection_sorted;
mod intersection_sorted_stream2;

use std::{borrow::Borrow, collections::BTreeSet, pin::Pin, task::ready};

use futures::{
	Stream,
	task::{Context, Poll},
};

pub use self::{
	difference_sorted_stream2::difference_sorted_stream2, intersection::intersection,
	intersection_sorted::intersection_sorted,
	intersection_sorted_stream2::intersection_sorted_stream2,
};

/// Reports whether `allowed` is empty or contains `item`.
///
/// An empty set acts as an unrestricted allowlist and selects every candidate.
#[inline]
#[must_use]
pub fn is_empty_or_contains<T, Q>(allowed: &BTreeSet<T>, item: &Q) -> bool
where
	T: Borrow<Q> + Ord,
	Q: Ord + ?Sized,
{
	allowed.is_empty() || allowed.contains(item)
}

/// Lazily fill the cached head of `stream` into `peeked` and return a borrow.
///
/// On entry, `Some(v)` in `peeked` is a previously-buffered head that has not
/// yet been consumed; `None` means the head must be pulled from the stream.
/// Returns `Ready(Some(&v))` when a head is available, `Ready(None)` once
/// `stream` is exhausted, or `Pending` when waiting on the stream.
fn poll_head<'p, S, T>(
	stream: Pin<&mut S>,
	peeked: &'p mut Option<T>,
	cx: &mut Context<'_>,
) -> Poll<Option<&'p T>>
where
	S: Stream<Item = T>,
{
	if peeked.is_none() {
		*peeked = ready!(stream.poll_next(cx));
	}

	Poll::Ready(peeked.as_ref())
}
