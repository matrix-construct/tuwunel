//! Trait BoolExt

use futures::future::OptionFuture;

/// Adds composable branching and conversion operations to Boolean values.
///
/// The adapters map conditions into options, results, futures, or selected
/// values. Lazy variants invoke their closure only on the documented branch.
pub trait BoolExt {
	/// Retains `t` only when the Boolean is true.
	///
	/// A false value produces `None` regardless of `t`. An existing `None`
	/// remains absent on either branch.
	fn and<T>(self, t: Option<T>) -> Option<T>;

	/// Computes the conjunction with another Boolean.
	///
	/// Both operands are already evaluated before the method runs. The result
	/// is true only when both values are true.
	#[must_use]
	fn and_is(self, b: bool) -> bool;

	/// Computes a conjunction with the closure's Boolean result.
	///
	/// The closure is invoked even when the receiver is false. Its result is
	/// then combined with the receiver using logical conjunction.
	#[must_use]
	fn and_if<F: FnOnce() -> bool>(self, f: F) -> bool;

	/// Invokes `f` and returns its option only when the Boolean is true.
	///
	/// A false receiver returns `None` without calling the closure. A true
	/// receiver preserves either option variant returned by `f`.
	fn and_then<T, F: FnOnce() -> Option<T>>(self, f: F) -> Option<T>;

	/// Clones `t` when true or returns `err` when false.
	///
	/// The referenced value is cloned only for the true branch. The owned
	/// fallback is returned for false and dropped for true.
	#[must_use]
	fn clone_or<T: Clone>(self, err: T, t: &T) -> T;

	/// Copies `t` when true or returns `err` when false.
	///
	/// Both candidate values are passed by value. The Boolean selects which
	/// copy becomes the result.
	#[must_use]
	fn copy_or<T: Copy>(self, err: T, t: T) -> T;

	/// Asserts that the Boolean is true and returns it.
	///
	/// A successful assertion always returns `true`. The supplied message is
	/// used only for a failed assertion.
	///
	/// # Panics
	///
	/// Panics with `msg` when the Boolean is false.
	#[must_use]
	fn expect(self, msg: &str) -> Self;

	/// Asserts that the Boolean is false and returns it.
	///
	/// A successful assertion always returns `false`. The supplied message is
	/// used only for a failed assertion.
	///
	/// # Panics
	///
	/// Panics with `msg` when the Boolean is true.
	#[must_use]
	fn expect_false(self, msg: &str) -> Self;

	/// Converts true to `Some(())` and false to `None`.
	///
	/// The unit value carries no additional state. The option is useful for
	/// continuing condition-driven combinator chains.
	fn into_option(self) -> Option<()>;

	/// Converts true to `Ok(())` and false to `Err(())`.
	///
	/// Both result payloads are unit values. The conversion preserves only the
	/// receiver's success or failure state.
	#[expect(clippy::result_unit_err)]
	fn into_result(self) -> Result<(), ()>;

	/// Reports whether the Boolean is false.
	///
	/// The receiver is borrowed rather than consumed. The returned value is the
	/// logical negation of the receiver.
	#[must_use]
	fn is_false(&self) -> Self;

	/// Applies `f` to the Boolean value.
	///
	/// The closure is invoked exactly once for either Boolean branch. This
	/// method allows a Boolean to begin a generic method chain.
	fn map<T, F: FnOnce(Self) -> T>(self, f: F) -> T
	where
		Self: Sized;

	/// Maps true through `f` or returns `err` for false.
	///
	/// The closure is called only for the true branch. The error is supplied
	/// eagerly, returned for false, and dropped for true.
	fn map_ok_or<T, E, F: FnOnce() -> T>(self, err: E, f: F) -> Result<T, E>;

	/// Maps true through `f` or returns `err` for false.
	///
	/// The closure is called only for the true branch. The fallback value is
	/// returned directly when the receiver is false.
	fn map_or<T, F: FnOnce() -> T>(self, err: T, f: F) -> T;

	/// Selects between lazy false and true branch closures.
	///
	/// `f` is invoked when the receiver is true, while `err` is invoked when it
	/// is false. Exactly one closure runs.
	fn map_or_else<T, E: FnOnce() -> T, F: FnOnce() -> T>(self, err: E, f: F) -> T;

	/// Converts true to `Ok(())` and false to the supplied error.
	///
	/// The error value is supplied eagerly and returned by the false branch.
	/// The true branch drops it and carries a unit success value.
	fn ok_or<E>(self, err: E) -> Result<(), E>;

	/// Converts true to `Ok(())` or lazily creates an error for false.
	///
	/// The closure is called only when the receiver is false. A true receiver
	/// produces the unit success value directly.
	fn ok_or_else<E, F: FnOnce() -> E>(self, err: F) -> Result<(), E>;

	/// Invokes `f` and returns its value only when the Boolean is false.
	///
	/// A true receiver returns `None` without calling the closure. It is the
	/// false-branch counterpart to `bool::then`.
	fn or<T, F: FnOnce() -> T>(self, f: F) -> Option<T>;

	/// Returns `Some(t)` when false and `None` when true.
	///
	/// The supplied value is returned for false and dropped for true. It is the
	/// inverse of `bool::then_some`.
	fn or_some<T>(self, t: T) -> Option<T>;

	/// Creates an optional future only when the Boolean is true.
	///
	/// The closure is not invoked for a false receiver. Awaiting the returned
	/// [`OptionFuture`] yields `Some(output)` when constructed or `None`
	/// otherwise.
	fn then_async<O: Future, F: FnOnce() -> O>(self, f: F) -> OptionFuture<O>;

	/// Produces an absent option for any Boolean value.
	///
	/// The receiver is consumed only to preserve chain shape. No value is
	/// constructed for either branch.
	fn then_none<T>(self) -> Option<T>;

	/// Returns `Ok(t)` when true or `Err(e)` when false.
	///
	/// Both values are supplied eagerly and the Boolean selects the result
	/// variant. Neither value is converted.
	fn then_ok_or<T, E>(self, t: T, e: E) -> Result<T, E>;

	/// Returns `Ok(t)` when true or lazily creates an error when false.
	///
	/// The error closure is called only for the false branch. The success value
	/// is moved into `Ok` when the receiver is true.
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
