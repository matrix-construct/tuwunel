#![expect(clippy::wrong_self_convention)]

use futures::Future;

use super::ReadyEqExt;

pub trait ReadyBoolExt
where
	Self: Future<Output = bool> + ReadyEqExt<bool> + Send,
{
	#[inline]
	fn is_false(self) -> impl Future<Output = bool> + Send { self.eq(&false) }

	#[inline]
	fn is_true(self) -> impl Future<Output = bool> + Send { self.eq(&true) }
}

impl<Fut> ReadyBoolExt for Fut where Fut: Future<Output = bool> + Send {}
