use std::pin::Pin;

use futures::{
	Stream,
	stream::FusedStream,
	task::{Context, Poll},
};
use tuwunel_core::Result;

use super::{Cursor, State, keyval_longevity};
use crate::keyval::KeyVal;

pub(crate) struct Items<'a> {
	state: State<'a>,
}

impl<'a> From<State<'a>> for Items<'a> {
	#[inline]
	fn from(state: State<'a>) -> Self {
		Self { state }
	}
}

impl<'a> Cursor<'a, KeyVal<'a>> for Items<'a> {
	#[inline]
	fn state(&self) -> &State<'a> {
		&self.state
	}

	#[inline]
	fn fetch(&self) -> Option<KeyVal<'a>> {
		self.state.fetch().map(keyval_longevity)
	}

	#[inline]
	fn seek(&mut self) {
		self.state.seek_fwd();
	}
}

impl<'a> Stream for Items<'a> {
	type Item = Result<KeyVal<'a>>;

	fn poll_next(mut self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		Poll::Ready(self.seek_and_get())
	}
}

impl FusedStream for Items<'_> {
	#[inline]
	fn is_terminated(&self) -> bool {
		!self.state.init && !self.state.valid()
	}
}
