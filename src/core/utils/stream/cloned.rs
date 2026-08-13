use futures::{Stream, StreamExt, stream::Map};

/// Clones items yielded by a stream of shared references.
///
/// Each referenced value is cloned only when its stream item is polled. Item
/// order and readiness follow the source stream unchanged.
pub trait Cloned<'a, T, S>
where
	S: Stream<Item = &'a T>,
	T: Clone + 'a,
{
	/// Returns a stream of owned clones from the borrowed items.
	///
	/// The adapter uses [`Clone::clone`] for each yielded reference. It
	/// performs no eager collection or buffering.
	fn cloned(self) -> Map<S, fn(&T) -> T>;
}

impl<'a, T, S> Cloned<'a, T, S> for S
where
	S: Stream<Item = &'a T>,
	T: Clone + 'a,
{
	#[inline]
	fn cloned(self) -> Map<S, fn(&T) -> T> { self.map(Clone::clone) }
}
