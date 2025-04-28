const QUOTE: char = '"';

/// Slice a string between quotes
pub trait Unquote<'a> {
	/// Whether the input is quoted. If this is false the fallible methods of
	/// this interface will fail.
	fn is_quoted(&self) -> bool;

	/// Unquotes a string. If the input is not quoted it is simply returned
	/// as-is. If the input is partially quoted on either end that quote is not
	/// removed.
	fn unquote(&self) -> Option<&'a str>;

	/// Unquotes a string. The input must be quoted on each side for Some to be
	/// returned
	fn unquote_infallible(&self) -> &'a str;
}

impl<'a> Unquote<'a> for &'a str {
	#[inline]
	fn unquote_infallible(&self) -> &'a str {
		self.strip_prefix(QUOTE)
			.unwrap_or(self)
			.strip_suffix(QUOTE)
			.unwrap_or(self)
	}

	#[inline]
	fn unquote(&self) -> Option<&'a str> {
		self.strip_prefix(QUOTE)
			.and_then(|s| s.strip_suffix(QUOTE))
	}

	#[inline]
	fn is_quoted(&self) -> bool {
		self.starts_with(QUOTE) && self.ends_with(QUOTE)
	}
}
