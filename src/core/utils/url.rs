//! Hostname and URL matching utilities.
//!
//! These helpers provide reusable matching rules that the URL parser does not
//! expose directly.

use std::fmt::{self, Display};

use http::Uri;

/// Reports whether a hostname is equal to or beneath a domain name.
///
/// Matching is ASCII case-insensitive and accepts a domain with an optional
/// leading dot. A suffix only matches at a DNS label boundary. A single dot
/// matches only a hostname with a trailing dot.
#[must_use]
pub fn hostname_matches_domain(hostname: &str, domain: &str) -> bool {
	if domain == "." {
		return hostname.ends_with('.');
	}

	let domain = domain.strip_prefix('.').unwrap_or(domain);

	if domain.is_empty() {
		return false;
	}

	if hostname.eq_ignore_ascii_case(domain) {
		return true;
	}

	let Some(separator) = hostname
		.len()
		.checked_sub(domain.len())
		.and_then(|index| index.checked_sub(1))
	else {
		return false;
	};

	let Some(suffix_start) = separator.checked_add(1) else {
		return false;
	};

	hostname.as_bytes().get(separator) == Some(&b'.')
		&& hostname
			.get(suffix_start..)
			.is_some_and(|suffix| suffix.eq_ignore_ascii_case(domain))
}

/// Formats a request URI without exposing sensitive components.
///
/// Query values are always replaced. A matched route template can replace
/// concrete path parameters when the caller has one.
pub struct SanitizedUri<'a> {
	uri: &'a Uri,
	path: Option<&'a str>,
}

impl<'a> SanitizedUri<'a> {
	/// Creates a redacted display wrapper for a request URI.
	///
	/// The original path is retained, while any query value is replaced.
	#[must_use]
	pub const fn new(uri: &'a Uri) -> Self { Self { uri, path: None } }

	/// Uses a route template instead of concrete path parameters.
	///
	/// Query values remain replaced when the URI contains a query.
	#[must_use]
	pub const fn with_path(uri: &'a Uri, path: &'a str) -> Self { Self { uri, path: Some(path) } }
}

impl Display for SanitizedUri<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let path = self.path.unwrap_or_else(|| self.uri.path());

		match self.uri.query() {
			| Some(_) => write!(f, "{path}?<redacted>"),
			| None => f.write_str(path),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn redacts_query() {
		let uri: Uri = "/_matrix/client/v3/sync?access_token=syt_realtoken"
			.parse()
			.expect("valid uri");

		let out = SanitizedUri::new(&uri).to_string();

		assert_eq!(out, "/_matrix/client/v3/sync?<redacted>");
		assert!(!out.contains("access_token"));
		assert!(!out.contains("syt_realtoken"));
	}

	#[test]
	fn preserves_path_without_query() {
		let uri: Uri = "/_matrix/client/v3/sync"
			.parse()
			.expect("valid uri");

		assert_eq!(SanitizedUri::new(&uri).to_string(), "/_matrix/client/v3/sync");
	}

	#[test]
	fn replaces_concrete_path_parameters() {
		let uri = Uri::builder()
			.path_and_query(
				"/_matrix/client/unstable/org.matrix.msc4140/delayed_events/secret?action=send",
			)
			.build()
			.expect("valid uri");

		let out = SanitizedUri::with_path(
			&uri,
			"/_matrix/client/unstable/org.matrix.msc4140/delayed_events/{delay_id}",
		)
		.to_string();

		assert_eq!(
			out,
			"/_matrix/client/unstable/org.matrix.msc4140/delayed_events/{delay_id}?<redacted>"
		);
		assert!(!out.contains("secret"));
	}
}
