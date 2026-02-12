#![expect(clippy::wrong_self_convention)]

use futures::{Future, FutureExt, future::OptionFuture};

use super::super::BoolExt;

pub trait OptionFutureExt<T> {
	fn is_none_or(self, f: impl FnOnce(&T) -> bool + Send) -> impl Future<Output = bool> + Send;

	fn is_some_and(self, f: impl FnOnce(&T) -> bool + Send) -> impl Future<Output = bool> + Send;

	fn unwrap_or(self, t: T) -> impl Future<Output = T> + Send;

	fn unwrap_or_default(self) -> impl Future<Output = T> + Send
	where
		T: Default;

	fn unwrap_or_else(self, f: impl FnOnce() -> T + Send) -> impl Future<Output = T> + Send;

	fn unwrap_or_else_async<F: Future<Output = T> + Send>(
		self,
		f: impl FnOnce() -> F + Send,
	) -> impl Future<Output = Option<T>> + Send;
}

impl<T, Fut> OptionFutureExt<T> for OptionFuture<Fut>
where
	Fut: Future<Output = T> + Send,
	T: Send,
{
	#[inline]
	fn is_none_or(self, f: impl FnOnce(&T) -> bool + Send) -> impl Future<Output = bool> + Send {
		self.map(|o| o.as_ref().is_none_or(f))
	}

	#[inline]
	fn is_some_and(self, f: impl FnOnce(&T) -> bool + Send) -> impl Future<Output = bool> + Send {
		self.map(|o| o.as_ref().is_some_and(f))
	}

	#[inline]
	fn unwrap_or(self, t: T) -> impl Future<Output = T> + Send { self.map(|o| o.unwrap_or(t)) }

	#[inline]
	fn unwrap_or_default(self) -> impl Future<Output = T> + Send
	where
		T: Default,
	{
		self.map(Option::unwrap_or_default)
	}

	#[inline]
	fn unwrap_or_else(self, f: impl FnOnce() -> T + Send) -> impl Future<Output = T> + Send {
		self.map(|o| o.unwrap_or_else(f))
	}

	#[inline]
	fn unwrap_or_else_async<F: Future<Output = T> + Send>(
		self,
		f: impl FnOnce() -> F + Send,
	) -> impl Future<Output = Option<T>> + Send {
		self.map(|o| o.is_none().then_async(f)).flatten()
	}
}
