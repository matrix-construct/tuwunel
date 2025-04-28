use std::clone::Clone;

use futures::{Stream, StreamExt, stream::Map};

pub trait Cloned<'a, T, S>
where
	S: Stream<Item = &'a T>,
	T: Clone + 'a,
{
	fn cloned(self) -> Map<S, fn(&T) -> T>;
}

impl<'a, T, S> Cloned<'a, T, S> for S
where
	S: Stream<Item = &'a T>,
	T: Clone + 'a,
{
	#[inline]
	fn cloned(self) -> Map<S, fn(&T) -> T> {
		self.map(Clone::clone)
	}
}
