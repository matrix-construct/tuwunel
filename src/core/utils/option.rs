//! Asynchronous mapping adapters for optional values.
//!
//! [`OptionExt`] turns an optional value into an optional future, a fallible
//! future of an option, or a zero-or-one stream. The mapping closure runs only
//! when the option contains a value.

use futures::{FutureExt, Stream, future::OptionFuture};

use super::IterStream;

/// Extends [`Option`] with asynchronous mapping adapters.
///
/// [`OptionExt::map_async`] preserves optionality through future completion.
/// [`OptionExt::map_stream`] exposes the mapped result as a zero-or-one stream.
/// [`OptionExt::try_map_async`] lifts a fallible future's error out of the
/// option.
pub trait OptionExt<T> {
	/// Maps the contained value to a future without eagerly awaiting it.
	///
	/// A [`Some`] value yields an [`OptionFuture`] that resolves to the mapped
	/// output. [`None`] resolves to `None` and never calls the mapping
	/// closure.
	fn map_async<F, Fut, U>(self, f: F) -> OptionFuture<Fut>
	where
		F: FnOnce(T) -> Fut,
		Fut: Future<Output = U> + Send,
		U: Send;

	/// Maps the contained value to a future-backed stream with at most one
	/// item.
	///
	/// A [`Some`] value yields the mapped output after its future completes.
	/// [`None`] yields an empty stream and never calls the mapping closure.
	#[inline]
	fn map_stream<F, Fut, U>(self, f: F) -> impl Stream<Item = U> + Send
	where
		F: FnOnce(T) -> Fut,
		Fut: Future<Output = U> + Send,
		U: Send,
		Self: Sized,
	{
		self.map_async(f)
			.map(Option::into_iter)
			.map(IterStream::stream)
			.flatten_stream()
	}

	/// Maps the contained value to a fallible future and lifts its error out.
	///
	/// A [`Some`] value resolves to the mapped output in `Ok(Some(_))`, or to
	/// the future's error. [`None`] resolves to `Ok(None)` and never calls the
	/// mapping closure.
	#[inline]
	fn try_map_async<F, Fut, U, E>(
		self,
		f: F,
	) -> impl Future<Output = Result<Option<U>, E>> + Send
	where
		F: FnOnce(T) -> Fut,
		Fut: Future<Output = Result<U, E>> + Send,
		U: Send,
		E: Send,
		Self: Sized,
	{
		self.map_async(f).map(Option::transpose)
	}
}

impl<T> OptionExt<T> for Option<T> {
	#[inline]
	fn map_async<F, Fut, U>(self, f: F) -> OptionFuture<Fut>
	where
		F: FnOnce(T) -> Fut,
		Fut: Future<Output = U> + Send,
		U: Send,
	{
		self.map(f).into()
	}
}
