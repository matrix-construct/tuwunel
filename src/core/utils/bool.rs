//! Trait BoolExt

use futures::future::OptionFuture;

/// Boolean extensions and chain.starters
pub trait BoolExt {
	fn and<T>(self, t: Option<T>) -> Option<T>;

	#[must_use]
	fn and_is(self, b: bool) -> bool;

	#[must_use]
	fn and_if<F: FnOnce() -> bool>(self, f: F) -> bool;

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

	#[expect(clippy::result_unit_err)]
	fn into_result(self) -> Result<(), ()>;

	#[must_use]
	fn is_false(&self) -> Self;

	fn map<T, F: FnOnce(Self) -> T>(self, f: F) -> T
	where
		Self: Sized;

	fn map_ok_or<T, E, F: FnOnce() -> T>(self, err: E, f: F) -> Result<T, E>;

	fn map_or<T, F: FnOnce() -> T>(self, err: T, f: F) -> T;

	fn map_or_else<T, E: FnOnce() -> T, F: FnOnce() -> T>(self, err: E, f: F) -> T;

	fn ok_or<E>(self, err: E) -> Result<(), E>;

	fn ok_or_else<E, F: FnOnce() -> E>(self, err: F) -> Result<(), E>;

	fn or<T, F: FnOnce() -> T>(self, f: F) -> Option<T>;

	fn or_some<T>(self, t: T) -> Option<T>;

	fn then_async<O: Future, F: FnOnce() -> O>(self, f: F) -> OptionFuture<O>;

	fn then_none<T>(self) -> Option<T>;

	fn then_ok_or<T, E>(self, t: T, e: E) -> Result<T, E>;

	fn then_ok_or_else<T, E, F: FnOnce() -> E>(self, t: T, e: F) -> Result<T, E>;
}

impl BoolExt for bool {
	#[inline]
	fn and<T>(self, t: Option<T>) -> Option<T> { self.then_some(t).flatten() }

	#[inline]
	fn and_if<F: FnOnce() -> Self>(self, f: F) -> Self { self.and_is(f()) }

	#[inline]
	fn and_is(self, b: Self) -> Self { self && b }

	#[inline]
	fn and_then<T, F: FnOnce() -> Option<T>>(self, f: F) -> Option<T> { self.then(f).flatten() }

	#[inline]
	fn clone_or<T: Clone>(self, err: T, t: &T) -> T { self.map_or(err, || t.clone()) }

	#[inline]
	fn copy_or<T: Copy>(self, err: T, t: T) -> T { self.map_or(err, || t) }

	#[inline]
	fn expect(self, msg: &str) -> Self { self.then_some(true).expect(msg) }

	#[inline]
	fn expect_false(self, msg: &str) -> Self { self.is_false().then_some(false).expect(msg) }

	#[inline]
	fn into_option(self) -> Option<()> { self.then_some(()) }

	#[inline]
	fn into_result(self) -> Result<(), ()> { self.ok_or(()) }

	#[inline]
	fn is_false(&self) -> Self { self.eq(&false) }

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
	fn map_or<T, F: FnOnce() -> T>(self, err: T, f: F) -> T { self.then(f).unwrap_or(err) }

	#[inline]
	fn map_or_else<T, E: FnOnce() -> T, F: FnOnce() -> T>(self, err: E, f: F) -> T {
		self.then(f).unwrap_or_else(err)
	}

	#[inline]
	fn ok_or<E>(self, err: E) -> Result<(), E> { self.into_option().ok_or(err) }

	#[inline]
	fn ok_or_else<E, F: FnOnce() -> E>(self, err: F) -> Result<(), E> {
		self.into_option().ok_or_else(err)
	}

	#[inline]
	fn or<T, F: FnOnce() -> T>(self, f: F) -> Option<T> { self.is_false().then(f) }

	#[inline]
	fn or_some<T>(self, t: T) -> Option<T> { self.is_false().then_some(t) }

	#[inline]
	fn then_async<O: Future, F: FnOnce() -> O>(self, f: F) -> OptionFuture<O> {
		OptionFuture::<_>::from(self.then(f))
	}

	#[inline]
	fn then_none<T>(self) -> Option<T> { Option::<T>::None }

	#[inline]
	fn then_ok_or<T, E>(self, t: T, e: E) -> Result<T, E> { self.map_ok_or(e, move || t) }

	#[inline]
	fn then_ok_or_else<T, E, F: FnOnce() -> E>(self, t: T, e: F) -> Result<T, E> {
		self.ok_or_else(e).map(move |()| t)
	}
}
