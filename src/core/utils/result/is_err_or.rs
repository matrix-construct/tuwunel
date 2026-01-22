#![expect(clippy::wrong_self_convention)]

use super::Result;

pub trait IsErrOr<T> {
	fn is_err_or<F: FnOnce(T) -> bool>(self, f: F) -> bool;
}

impl<T, E> IsErrOr<T> for Result<T, E> {
	#[inline]
	fn is_err_or<F>(self, f: F) -> bool
	where
		F: FnOnce(T) -> bool,
	{
		if let Ok(t) = self { f(t) } else { true }
	}
}
