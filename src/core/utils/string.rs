//! String conversion, formatting, parsing, serialization, and slicing
//! utilities.
//!
//! The module includes borrowed unquoted views, Serde adapters, chunking, case
//! conversion, deterministic prefix selection, and UTF-8 conversion.
//! Specialized display wrappers avoid allocation. String traits and helpers
//! are re-exported from child modules.

mod between;
mod chunk;

pub mod de;

mod split;
mod tests;
mod unquote;
mod unquoted;

use std::{
	borrow::Cow,
	fmt::{self, write},
	io,
	mem::replace,
	ops::Range,
	str::from_utf8,
};

pub use self::{
	between::Between, chunk::chunk, split::SplitInfallible, unquote::Unquote, unquoted::Unquoted,
};
use crate::{Result, arrayvec::ArrayString, smallstr::SmallString};

/// Provides a shared empty string slice.
///
/// The value has static lifetime and is suitable for default or fallback
/// references. It is identical to the empty string literal.
pub const EMPTY: &str = "";

/// Formats arguments into a small string with an inline byte capacity.
///
/// Arguments are forwarded to [`format_small_string`] without an intermediate
/// `String` allocation. Inline capacity is inferred from the expected type at
/// the call site, and output beyond it spills to the heap.
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! format_small_string {
	($($args:tt)+) => {
		$crate::utils::string::format_small_string(std::format_args!($($args)+))
	};
}

/// Formats arguments into an array string with constant byte capacity.
///
/// Arguments are forwarded to [`format_array_string`] without any allocation.
/// Capacity is inferred from the expected type at the call site, and output
/// beyond it panics.
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! format_array_string {
	($($args:tt)+) => {
		$crate::utils::string::format_array_string(std::format_args!($($args)+))
	};
}

/// Formats a literal only when placeholders appear to be present.
///
/// With one argument, a literal containing both `{` and `}` is formatted; any
/// other literal is converted directly through `Into`. With additional
/// arguments, the first literal is always treated as a format string.
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! format_maybe {
	($s:literal $(,)?) => {
		if $crate::is_format!($s) { std::format!($s).into() } else { $s.into() }
	};

	($s:literal, $($args:tt)+) => {
		std::format!($s, $($args)+).into()
	};
}

/// Tests whether a string literal appears to contain a formatting placeholder.
///
/// A literal returns `true` only when it contains at least one `{` and at least
/// one `}`. Every other token pattern returns `false`, so the result is a
/// heuristic rather than syntax validation.
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! is_format {
	($s:literal) => {
		::const_str::contains!($s, "{") && ::const_str::contains!($s, "}")
	};

	($($s:tt)+) => {
		false
	};
}

/// Collects text emitted by a callback into an owned string.
///
/// The callback receives a formatting writer backed by a new `String`. Its
/// error is propagated, and the buffer is returned only after the callback
/// succeeds.
#[inline]
pub fn collect_stream<F>(func: F) -> Result<String>
where
	F: FnOnce(&mut dyn fmt::Write) -> Result,
{
	let mut out = String::new();
	func(&mut out)?;

	Ok(out)
}

/// Converts ASCII camel-case text into an owned lowercase snake-case string.
///
/// An underscore is inserted before an uppercase byte only when the preceding
/// byte is not uppercase; leading and consecutive capitals remain joined. Input
/// is processed bytewise, so non-ASCII UTF-8 text is not preserved.
#[inline]
#[must_use]
pub fn camel_to_snake_string(s: &str) -> String {
	let est_len = s
		.chars()
		.fold(s.len(), |est, c| est.saturating_add(usize::from(c.is_ascii_uppercase())));

	let mut ret = String::with_capacity(est_len);
	camel_to_snake_case(&mut ret, s.as_bytes()).expect("string-to-string stream error");
	ret
}

/// Streams ASCII camel-case bytes into a writer as lowercase snake case.
///
/// The underscore rule matches [`camel_to_snake_string`]. The first input read
/// error stops processing and is not returned, while output formatting errors
/// are propagated. Bytes outside ASCII are written as individual Unicode code
/// points rather than decoded as UTF-8.
#[inline]
#[expect(clippy::unbuffered_bytes)] // these are allocated string utilities, not file I/O utils
pub fn camel_to_snake_case<I, O>(output: &mut O, input: I) -> Result
where
	I: io::Read,
	O: fmt::Write,
{
	let mut state = false;
	input
		.bytes()
		.take_while(Result::is_ok)
		.map(Result::unwrap)
		.map(char::from)
		.try_for_each(|ch| {
			let m = ch.is_ascii_uppercase();
			let s = replace(&mut state, !m);
			if m && s {
				output.write_char('_')?;
			}

			output.write_char(ch.to_ascii_lowercase())?;

			Result::<()>::Ok(())
		})
}

/// Returns the longest common ASCII prefix of a collection of strings.
///
/// The result borrows from the first entry and is empty when the collection is
/// empty or shares no prefix. Inputs are expected to be ASCII because some
/// non-ASCII prefixes can produce an invalid byte boundary and panic.
#[must_use]
#[expect(clippy::string_slice)]
pub fn common_prefix<T: AsRef<str>>(choice: &[T]) -> &str {
	choice.first().map_or(EMPTY, move |best| {
		choice
			.iter()
			.skip(1)
			.fold(best.as_ref(), |best, choice| {
				&best[0..choice
					.as_ref()
					.char_indices()
					.zip(best.char_indices())
					.take_while(|&(a, b)| a == b)
					.count()]
			})
	})
}

/// Returns a deterministic prefix selected from the string contents.
///
/// The candidate index is the wrapping sum of input bytes modulo the byte
/// length, with empty input using a modulus of one. When a range is supplied,
/// the index is clamped inclusively between `range.start` and `range.end`;
/// reversed endpoints panic. The index is then interpreted as a
/// character position, or the full string is returned when that position does
/// not exist. Because selection uses byte length but truncation uses character
/// count, non-ASCII input more often falls back to the full string.
#[inline]
#[must_use]
#[expect(clippy::arithmetic_side_effects)]
pub fn truncate_deterministic(str: &str, range: Option<Range<usize>>) -> &str {
	let range = range.unwrap_or(0..str.len());
	let len = str
		.as_bytes()
		.iter()
		.copied()
		.map(Into::into)
		.fold(0_usize, usize::wrapping_add)
		.wrapping_rem(str.len().max(1))
		.clamp(range.start, range.end);

	str.char_indices()
		.nth(len)
		.map(|(i, _)| str.split_at(i).0)
		.unwrap_or(str)
}

/// Displays a value into a small string with an inline byte capacity.
///
/// Output beyond `CAP` bytes spills to the heap. Panics if the value's
/// `Display` implementation returns a formatting error.
#[inline]
#[must_use]
pub fn to_small_string<const CAP: usize, T>(t: T) -> SmallString<[u8; CAP]>
where
	T: fmt::Display,
{
	format_small_string(format_args!("{t}"))
}

/// Formats arguments into a small string with an inline byte capacity.
///
/// Output beyond `CAP` bytes spills to the heap. Panics if formatting the
/// arguments fails; [`try_format_small_string`] returns that error instead.
#[inline]
#[must_use]
pub fn format_small_string<const CAP: usize>(args: fmt::Arguments<'_>) -> SmallString<[u8; CAP]> {
	try_format_small_string(args).expect("Failed to format into SmallString")
}

/// Formats arguments into a small string, propagating any formatting error.
///
/// Output beyond `CAP` bytes spills to the heap, so the buffer itself never
/// overflows. Only a `Display` implementation among the arguments can fail.
#[inline]
pub fn try_format_small_string<const CAP: usize>(
	args: fmt::Arguments<'_>,
) -> Result<SmallString<[u8; CAP]>> {
	let mut ret = SmallString::<[u8; CAP]>::new();
	write(&mut ret, args)?;

	Ok(ret)
}

/// Displays a value into an array string with constant byte capacity.
///
/// Output is confined to the stack, so `CAP` must bound the formatted length.
/// Panics when the output exceeds it, or when the value's `Display`
/// implementation returns a formatting error.
#[inline]
#[must_use]
pub fn to_array_string<const CAP: usize, T>(t: T) -> ArrayString<CAP>
where
	T: fmt::Display,
{
	format_array_string(format_args!("{t}"))
}

/// Formats arguments into an array string with constant byte capacity.
///
/// Output is confined to the stack, so `CAP` must bound the formatted length.
/// Panics when the output exceeds it or the arguments fail to format;
/// [`try_format_array_string`] returns both as an error.
#[inline]
#[must_use]
pub fn format_array_string<const CAP: usize>(args: fmt::Arguments<'_>) -> ArrayString<CAP> {
	try_format_array_string(args).expect("Failed to format into ArrayString")
}

/// Formats arguments into an array string, propagating any formatting error.
///
/// Output is confined to the stack. Exceeding `CAP` bytes yields a formatting
/// error, indistinguishable from a failure in an argument's `Display`
/// implementation.
#[inline]
pub fn try_format_array_string<const CAP: usize>(
	args: fmt::Arguments<'_>,
) -> Result<ArrayString<CAP>> {
	let mut ret = ArrayString::<CAP>::new();
	write(&mut ret, args)?;

	Ok(ret)
}

/// Converts UTF-8 bytes into an owned string.
///
/// Valid input is copied into a new `String`. Invalid UTF-8 is returned as an
/// error.
pub fn string_from_bytes(bytes: &[u8]) -> Result<String> {
	let str: &str = str_from_bytes(bytes)?;

	Ok(str.to_owned())
}

/// Borrows a byte slice as UTF-8 text.
///
/// Valid input is returned without allocation. Invalid UTF-8 is returned as an
/// error.
#[inline]
pub fn str_from_bytes(bytes: &[u8]) -> Result<&str> { Ok(from_utf8(bytes)?) }

/// Escapes a value for one markdown table cell.
///
/// Backslashes and pipes are escaped, and control characters and Unicode line
/// or paragraph separators become spaces, so the value cannot end its cell or
/// row. A value needing no change is borrowed.
#[must_use]
pub fn markdown_cell(value: &str) -> Cow<'_, str> {
	let breaks_cell = |character: char| {
		character.is_control() || matches!(character, '\\' | '|' | '\u{2028}' | '\u{2029}')
	};

	let escape = |mut escaped: String, character: char| {
		match character {
			| '\\' => escaped.push_str("\\\\"),
			| '|' => escaped.push_str("\\|"),
			| _ if breaks_cell(character) => escaped.push(' '),
			| _ => escaped.push(character),
		}

		escaped
	};

	value
		.contains(breaks_cell)
		.then(|| {
			value
				.chars()
				.fold(String::with_capacity(value.len()), escape)
		})
		.map_or(Cow::Borrowed(value), Cow::Owned)
}

/// Picks the singular or plural form of a noun for a count.
///
/// Only a count of exactly one takes the singular, so zero reads as plural.
#[inline]
#[must_use]
pub fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
	match count {
		| 1 => one,
		| _ => many,
	}
}
