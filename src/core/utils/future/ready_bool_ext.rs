#![expect(clippy::wrong_self_convention)]

use super::ReadyEqExt;

/// Tests the Boolean output of a future without awaiting it early.
///
/// The helpers attach equality comparisons to the future. Each returned future
/// resolves to the requested truth-state test.
pub trait ReadyBoolExt
where
	Self: Future<Output = bool> + ReadyEqExt<bool> + Send,
{
	/// Reports whether the future resolves to false.
	///
	/// The receiver is consumed, then its output is compared with `false` after
	/// completion.
	#[inline]
	fn is_false(self) -> impl Future<Output = bool> + Send { self.eq(&false) }

	/// Reports whether the future resolves to true.
	///
	/// The receiver is consumed, then its output is compared with `true` after
	/// completion.
	#[inline]
	fn is_true(self) -> impl Future<Output = bool> + Send { self.eq(&true) }
}

impl<Fut> ReadyBoolExt for Fut where Fut: Future<Output = bool> + Send {}
