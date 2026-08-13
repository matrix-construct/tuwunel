use super::Result;

/// Flattens optional and fallible values while discarding their original error.
///
/// Implementations cover both `Option<Result<T, E>>` and
/// `Result<Option<T>, E>`. Missing values and errors both become absence before
/// an optional replacement error is applied.
pub trait FlatOk<T> {
	/// Equivalent to .transpose().ok().flatten()
	fn flat_ok(self) -> Option<T>;

	/// Equivalent to .transpose().ok().flatten().ok_or(...)
	fn flat_ok_or<E>(self, err: E) -> Result<T, E>;

	/// Equivalent to .transpose().ok().flatten().ok_or_else(...)
	fn flat_ok_or_else<E, F: FnOnce() -> E>(self, err: F) -> Result<T, E>;
}

impl<T, E> FlatOk<T> for Option<Result<T, E>> {
	#[inline]
	fn flat_ok(self) -> Option<T> { self.transpose().ok().flatten() }

	#[inline]
	fn flat_ok_or<Ep>(self, err: Ep) -> Result<T, Ep> { self.flat_ok().ok_or(err) }

	#[inline]
	fn flat_ok_or_else<Ep, F: FnOnce() -> Ep>(self, err: F) -> Result<T, Ep> {
		self.flat_ok().ok_or_else(err)
	}
}

impl<T, E> FlatOk<T> for Result<Option<T>, E> {
	#[inline]
	fn flat_ok(self) -> Option<T> { self.ok().flatten() }

	#[inline]
	fn flat_ok_or<Ep>(self, err: Ep) -> Result<T, Ep> { self.flat_ok().ok_or(err) }

	#[inline]
	fn flat_ok_or_else<Ep, F: FnOnce() -> Ep>(self, err: F) -> Result<T, Ep> {
		self.flat_ok().ok_or_else(err)
	}
}
