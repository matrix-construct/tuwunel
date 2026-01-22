use std::fmt;

/// Debug-formats the given slice, but only up to the first `max_len` elements.
/// Any further elements are replaced by an ellipsis.
///
/// See also [`slice_truncated()`],
pub struct TruncatedSlice<'a, T> {
	inner: &'a [T],
	max_len: usize,
}

/// Debug-formats the given str, but only up to the first `max_len` elements.
/// Any further elements are replaced by an ellipsis.
///
/// See also [`str_truncated()`],
pub struct TruncatedStr<'a> {
	inner: &'a str,
	max_len: usize,
}

/// See [`TruncatedSlice`]. Useful for `#[instrument]`:
///
/// ```
/// use tuwunel_core::utils::debug::slice_truncated;
///
/// #[tracing::instrument(fields(foos = slice_truncated(foos, 42)))]
/// fn bar(foos: &[&str]) {}
/// ```
pub fn slice_truncated<T: fmt::Debug>(
	slice: &[T],
	max_len: usize,
) -> tracing::field::DebugValue<TruncatedSlice<'_, T>> {
	tracing::field::debug(TruncatedSlice { inner: slice, max_len })
}

/// See [`TruncatedStr`]. Useful for `#[instrument]`:
///
/// ```
/// use tuwunel_core::utils::debug::str_truncated;
///
/// #[tracing::instrument(fields(foos = str_truncated(foos, 42)))]
/// fn bar(foos: &str) {}
/// ```
#[must_use]
pub fn str_truncated(s: &str, max_len: usize) -> tracing::field::DebugValue<TruncatedStr<'_>> {
	tracing::field::debug(TruncatedStr { inner: s, max_len })
}

impl<T: fmt::Debug> fmt::Debug for TruncatedSlice<'_, T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if self.inner.len() <= self.max_len {
			write!(f, "{:?}", self.inner)
		} else {
			f.debug_list()
				.entries(&self.inner[..self.max_len])
				.entry(&"...")
				.finish()
		}
	}
}

impl fmt::Debug for TruncatedStr<'_> {
	#[expect(clippy::string_slice)]
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if self.inner.len() <= self.max_len {
			write!(f, "{:?}", self.inner)
		} else {
			let len = self
				.inner
				.char_indices()
				.skip_while(|(i, _)| *i < self.max_len)
				.map(|(i, _)| i)
				.next()
				.expect("At least one char_indice >= len for str");

			write!(f, "{:?}...", &self.inner[..len])
		}
	}
}
