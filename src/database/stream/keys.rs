use std::pin::Pin;

use futures::{
	Stream,
	stream::FusedStream,
	task::{Context, Poll},
};
use tuwunel_core::Result;

use super::{Cursor, State, slice_longevity};
use crate::keyval::Key;

pub(crate) struct Keys<'a> {
	state: State<'a>,
}

impl<'a> From<State<'a>> for Keys<'a> {
	#[inline]
	fn from(state: State<'a>) -> Self {
		Self { state }
	}
}

impl<'a> Cursor<'a, Key<'a>> for Keys<'a> {
	#[inline]
	fn state(&self) -> &State<'a> {
		&self.state
	}

	#[inline]
	fn fetch(&self) -> Option<Key<'a>> {
		self.state.fetch_key().map(slice_longevity)
	}

	#[inline]
	fn seek(&mut self) {
		self.state.seek_fwd();
	}
}

impl<'a> Stream for Keys<'a> {
	type Item = Result<Key<'a>>;

	fn poll_next(mut self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		Poll::Ready(self.seek_and_get())
	}
}

impl FusedStream for Keys<'_> {
	#[inline]
	fn is_terminated(&self) -> bool {
		!self.state.init && !self.state.valid()
	}
}
