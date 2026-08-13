use std::pin::Pin;

use futures::{
	Stream,
	stream::FusedStream,
	task::{Context, Poll},
};
use tuwunel_core::Result;

use super::{Cursor, State, keyval_longevity};
use crate::keyval::KeyVal;

/// Streams key-value pairs in descending RocksDB order.
///
/// The first poll uses a prepared position or seeks to the last key, while
/// later polls retreat before fetching. Both slices borrow cursor storage and
/// remain valid only until the next cursor movement.
pub(crate) struct ItemsRev<'a> {
	state: State<'a>,
}

impl<'a> From<State<'a>> for ItemsRev<'a> {
	#[inline]
	fn from(state: State<'a>) -> Self { Self { state } }
}

impl<'a> Cursor<'a, KeyVal<'a>> for ItemsRev<'a> {
	#[inline]
	fn state(&self) -> &State<'a> { &self.state }

	#[inline]
	fn state_mut(&mut self) -> &mut State<'a> { &mut self.state }

	#[inline]
	fn count(&self) -> (usize, Option<usize>) { self.state().count_rev() }

	#[inline]
	fn fetch(&self) -> Option<KeyVal<'a>> { self.state().fetch().map(keyval_longevity) }

	#[inline]
	fn seek(&mut self) { self.state_mut().seek_rev(); }
}

impl<'a> Stream for ItemsRev<'a> {
	type Item = Result<KeyVal<'a>>;

	fn poll_next(mut self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		Poll::Ready(self.seek_and_get())
	}

	fn size_hint(&self) -> (usize, Option<usize>) { self.count() }
}

impl FusedStream for ItemsRev<'_> {
	#[inline]
	fn is_terminated(&self) -> bool { !self.state().init && !self.state().valid() }
}
