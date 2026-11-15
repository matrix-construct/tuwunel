//! Hostname and URL matching utilities.
//!
//! These helpers provide reusable matching rules that the URL parser does not
//! expose directly.

use std::fmt::{self, Display, Write};

use http::Uri;
use url::{Host, Url};

use super::string::is_unreserved_byte;

/// Percent-encodes a string for use in a URL component.
///
/// ASCII letters, digits and unreserved punctuation remain unchanged.
/// Other UTF-8 bytes are encoded with uppercase hexadecimal digits.
#[must_use]
pub fn url_encode(s: &str) -> String {
	s.bytes()
		.fold(String::with_capacity(s.len()), |mut out, b| {
			if is_unreserved_byte(b) {
				out.push(b.into());
			} else {
				write!(&mut out, "%{b:02X}").ok();
			}

			out
		})
}

/// Whether a URI's host, or its scheme when it has no host, appears in `allowed`.
///
/// A URI with no host is the RFC 8252 §7.1 private-use form, as in
/// `io.element.android:/callback`. The comparison ignores case, and an
/// unparsable URI never matches.
#[must_use]
pub fn redirect_allowlisted(allowed: &[String], uri: &str) -> bool {
	Url::parse(uri).is_ok_and(|url| {
		let name = url.host_str().unwrap_or_else(|| url.scheme());

		allowed
			.iter()
			.any(|entry| entry.eq_ignore_ascii_case(name))
	})
}

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

/// Whether a URL's host names the loopback interface.
///
/// That is any IPv4 or IPv6 loopback address (RFC 8252 §7.3) or the domain
/// `localhost` (§8.3), compared without regard to case.
#[must_use]
pub fn is_loopback(url: &Url) -> bool {
	match url.host() {
		| Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
		| Some(Host::Ipv4(ip)) => ip.is_loopback(),
		| Some(Host::Ipv6(ip)) => ip.is_loopback(),
		| None => false,
	}
}

/// Whether a raw URL path survives URL parsing as written.
///
/// It holds only RFC 3986 path bytes and no dot segment, even once
/// percent-decoded. A URL parser rewrites anything else: it turns a backslash
/// into a separator and resolves dot segments, so a path failing this can
/// land elsewhere than it reads.
#[must_use]
pub fn is_plain_path(path: &str) -> bool {
	path.bytes().all(is_path_byte) && !has_dot_segment(path)
}

/// Whether a byte may appear unencoded in a URL path (RFC 3986).
///
/// That is an unreserved character, a sub-delimiter, `:`, `@`, `/`, or the
/// `%` opening a percent-encoded octet. A query or fragment delimiter does
/// not qualify, nor does a space, a backslash or a control byte.
#[must_use]
pub fn is_path_byte(byte: u8) -> bool {
	is_unreserved_byte(byte) || b"%!$&'()*+,;=:@/".contains(&byte)
}

/// Drops one trailing slash, if present.
///
/// Only one goes, so `a//` becomes `a/`.
#[must_use]
pub fn strip_trailing_slash(value: &str) -> &str { value.strip_suffix('/').unwrap_or(value) }

/// Whether a URL path holds a `.` or `..` segment once percent-decoded.
///
/// An encoded slash separates segments as a literal one does, so `%2e%2E`
/// and `a%2F..` both count. Only one level of encoding is decoded.
#[must_use]
pub fn has_dot_segment(path: &str) -> bool {
	path.split('/')
		.flat_map(|part| part.split("%2F"))
		.flat_map(|part| part.split("%2f"))
		.any(is_dot_segment)
}

fn is_dot_segment(segment: &str) -> bool {
	strip_dot(segment).is_some_and(|rest| rest.is_empty() || strip_dot(rest) == Some(""))
}

fn strip_dot(segment: &str) -> Option<&str> {
	segment.strip_prefix('.').or_else(|| {
		segment
			.get(..3)
			.filter(|unit| unit.eq_ignore_ascii_case("%2e"))
			.and_then(|_| segment.get(3..))
	})
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
	fn dot_segments_are_found_through_one_encoding() {
		for path in [
			"/a/./b",
			"/a/../b",
			"/a/..",
			"..",
			"/a/%2e%2e/b",
			"/a/%2E./b",
			"/a/.%2e",
			"/a/%2e/b",
			"/a%2F..%2Fb",
			"/a%2f.",
		] {
			assert!(has_dot_segment(path), "{path}");
		}

		for path in [
			"/a/b",
			"/a/.../b",
			"/a/.b/c",
			"/a/b.",
			"/a/%252e%252e/b",
			"/a/%2e%2e%2e",
			"/a/%2",
			"",
			"/",
		] {
			assert!(!has_dot_segment(path), "{path}");
		}
	}

	#[test]
	fn path_bytes_exclude_query_fragment_and_whitespace() {
		assert!(
			"/_matrix/federation/v1/a%20b:c@d!$&'()*+,;=-._~"
				.bytes()
				.all(is_path_byte)
		);

		for byte in [b'?', b'#', b' ', b'\\', b'"', b'<', b'^', b'`', b'{', b'|', 0, 0x7F, 0x80] {
			assert!(!is_path_byte(byte), "{byte:#x}");
		}
	}

	#[test]
	fn plain_paths_hold_no_rewritable_byte_or_segment() {
		assert!(is_plain_path("/_matrix/client/v1/rtc/livekit/get_token"));
		assert!(is_plain_path("/a/%2Fb/.c"));

		for path in ["/a/../b", "/a/%2e%2e/b", "/a/x\\..\\..\\b", "/a b", "/a?b", "/a#b"] {
			assert!(!is_plain_path(path), "{path}");
		}
	}

	#[test]
	fn one_trailing_slash_is_stripped() {
		for (value, expected) in [("a", "a"), ("a/", "a"), ("a//", "a/"), ("/", ""), ("", "")] {
			assert_eq!(strip_trailing_slash(value), expected, "{value}");
		}
	}

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
