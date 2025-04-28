//! Future extension for Partial Equality against present value

use futures::{Future, FutureExt};

pub trait ReadyEqExt<T>
where
	Self: Future<Output = T> + Send + Sized,
	T: PartialEq + Send + Sync,
{
	fn eq(self, t: &T) -> impl Future<Output = bool> + Send;

	fn ne(self, t: &T) -> impl Future<Output = bool> + Send;
}

impl<Fut, T> ReadyEqExt<T> for Fut
where
	Fut: Future<Output = T> + Send + Sized,
	T: PartialEq + Send + Sync,
{
	#[inline]
	fn eq(self, t: &T) -> impl Future<Output = bool> + Send {
		self.map(move |r| r.eq(t))
	}

	#[inline]
	fn ne(self, t: &T) -> impl Future<Output = bool> + Send {
		self.map(move |r| r.ne(t))
	}
}
