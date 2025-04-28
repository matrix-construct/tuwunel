//! Trait BoolExt

/// Boolean extensions and chain.starters
pub trait BoolExt {
	fn and<T>(self, t: Option<T>) -> Option<T>;

	fn and_then<T, F: FnOnce() -> Option<T>>(self, f: F) -> Option<T>;

	#[must_use]
	fn clone_or<T: Clone>(self, err: T, t: &T) -> T;

	#[must_use]
	fn copy_or<T: Copy>(self, err: T, t: T) -> T;

	#[must_use]
	fn expect(self, msg: &str) -> Self;

	#[must_use]
	fn expect_false(self, msg: &str) -> Self;

	fn into_option(self) -> Option<()>;

	#[allow(clippy::result_unit_err)]
	fn into_result(self) -> Result<(), ()>;

	fn map<T, F: FnOnce(Self) -> T>(self, f: F) -> T
	where
		Self: Sized;

	fn map_ok_or<T, E, F: FnOnce() -> T>(self, err: E, f: F) -> Result<T, E>;

	fn map_or<T, F: FnOnce() -> T>(self, err: T, f: F) -> T;

	fn map_or_else<T, F: FnOnce() -> T>(self, err: F, f: F) -> T;

	fn ok_or<E>(self, err: E) -> Result<(), E>;

	fn ok_or_else<E, F: FnOnce() -> E>(self, err: F) -> Result<(), E>;

	fn or<T, F: FnOnce() -> T>(self, f: F) -> Option<T>;

	fn or_some<T>(self, t: T) -> Option<T>;
}

impl BoolExt for bool {
	#[inline]
	fn and<T>(self, t: Option<T>) -> Option<T> {
		self.then_some(t).flatten()
	}

	#[inline]
	fn and_then<T, F: FnOnce() -> Option<T>>(self, f: F) -> Option<T> {
		self.then(f).flatten()
	}

	#[inline]
	fn clone_or<T: Clone>(self, err: T, t: &T) -> T {
		self.map_or(err, || t.clone())
	}

	#[inline]
	fn copy_or<T: Copy>(self, err: T, t: T) -> T {
		self.map_or(err, || t)
	}

	#[inline]
	fn expect(self, msg: &str) -> Self {
		self.then_some(true).expect(msg)
	}

	#[inline]
	fn expect_false(self, msg: &str) -> Self {
		(!self).then_some(false).expect(msg)
	}

	#[inline]
	fn into_option(self) -> Option<()> {
		self.then_some(())
	}

	#[inline]
	fn into_result(self) -> Result<(), ()> {
		self.ok_or(())
	}

	#[inline]
	fn map<T, F: FnOnce(Self) -> T>(self, f: F) -> T
	where
		Self: Sized,
	{
		f(self)
	}

	#[inline]
	fn map_ok_or<T, E, F: FnOnce() -> T>(self, err: E, f: F) -> Result<T, E> {
		self.ok_or(err).map(|()| f())
	}

	#[inline]
	fn map_or<T, F: FnOnce() -> T>(self, err: T, f: F) -> T {
		self.then(f).unwrap_or(err)
	}

	#[inline]
	fn map_or_else<T, F: FnOnce() -> T>(self, err: F, f: F) -> T {
		self.then(f).unwrap_or_else(err)
	}

	#[inline]
	fn ok_or<E>(self, err: E) -> Result<(), E> {
		self.into_option().ok_or(err)
	}

	#[inline]
	fn ok_or_else<E, F: FnOnce() -> E>(self, err: F) -> Result<(), E> {
		self.into_option().ok_or_else(err)
	}

	#[inline]
	fn or<T, F: FnOnce() -> T>(self, f: F) -> Option<T> {
		(!self).then(f)
	}

	#[inline]
	fn or_some<T>(self, t: T) -> Option<T> {
		(!self).then_some(t)
	}
}
