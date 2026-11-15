//! MSC4512: the paths an appservice claims and the requests forwarded to it.
//!
//! The claim grammar is shared with `fed_proxy`; the send goes through the
//! appservice's own client pool, so a proxy URL takes the forms `url` does.

/// MSC4512: the API a proxied path belongs to.
///
/// It decides how the forwarded request is authenticated and which identity
/// header carries the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Root {
	/// `/_matrix/client/`, authenticated by an access token.
	Client,

	/// `/_matrix/federation/`, authenticated by a server signature.
	Federation,
}

/// MSC4512: splits a raw request path into its API and the remainder after
/// the version.
///
/// The version is one `v<x>` segment or the two segments `unstable/<ns>`.
/// The remainder is what a proxy claim is matched against.
#[must_use]
pub fn split_path(path: &str) -> Option<(Root, &str)> {
	let (root, versioned) = path
		.strip_prefix("/_matrix/client/")
		.map(|rest| (Root::Client, rest))
		.or_else(|| {
			path.strip_prefix("/_matrix/federation/")
				.map(|rest| (Root::Federation, rest))
		})?;

	let (version, rest) = versioned.split_once('/')?;
	let remainder = match version {
		| "unstable" => rest
			.split_once('/')
			.filter(|(namespace, _)| !namespace.is_empty())
			.map(|(_, remainder)| remainder)?,
		| _ if version.len() > 1 && version.starts_with('v') => rest,
		| _ => return None,
	};

	Some((root, remainder))
}
