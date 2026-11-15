use ruma::{OwnedUserId, ServerName, UserId, api::appservice::Registration};
use tuwunel_core::{Err, Result, err, implement, utils::url::strip_trailing_slash};
use url::Url;

use super::{NamespaceRegex, destination, socket_path};

/// MSC4502: the scope granting the targeted room membership query.
///
/// An appservice whose registration requests it may ask whether any user or
/// server is joined to any room this server is in.
pub const IS_JOINED_SCOPE: &str = "urn:matrix:client:io.element.msc4502:rooms:is_joined";

/// MSC4512: the path prefixes an appservice may claim for proxying.
///
/// A claim must equal one of them. The proxy also lets the appservice send
/// federation requests as this server under its prefix, so the list stays
/// closed.
pub const PROXY_PREFIXES: [&str; 1] = ["rtc/livekit"];

/// Appservice registration combined with its compiled regular expressions.
#[derive(Clone, Debug)]
pub struct RegistrationInfo {
	pub aliases: NamespaceRegex,
	pub users: NamespaceRegex,
	pub rooms: NamespaceRegex,
	pub sender: OwnedUserId,

	/// MSC4512: the allowed prefix the registration validly claims.
	///
	/// `None` for no claim, an invalid one, or one another appservice holds.
	pub proxy_prefix: Option<&'static str>,

	pub registration: Registration,
}

impl RegistrationInfo {
	/// Validates an application service registration.
	///
	/// Checks its URL and sender, and compiles its namespace expressions. An
	/// invalid proxy claim is left out rather than refused; [`proxy_claim`]
	/// reports why.
	pub fn new(registration: Registration, server_name: &ServerName) -> Result<Self> {
		if socket_path(registration.url.as_deref().unwrap_or_default())?.is_some()
			&& cfg!(not(unix))
		{
			return Err!("Unix sockets are not supported on this platform");
		}

		Ok(Self {
			aliases: NamespaceRegex::new(false, registration.namespaces.aliases.iter())?,
			users: NamespaceRegex::new(false, registration.namespaces.users.iter())?,
			rooms: NamespaceRegex::new(true, registration.namespaces.rooms.iter())?,
			sender: OwnedUserId::from_parts(
				'@',
				registration.sender_localpart.as_ref(),
				server_name.as_str().into(),
			)?,
			proxy_prefix: proxy_claim(&registration).unwrap_or_default(),

			registration,
		})
	}

	/// MSC3905: the `users` regex matches local users only.
	#[inline]
	#[must_use]
	pub fn is_user_match(&self, user_id: &UserId) -> bool {
		user_id == self.sender
			|| (self.users.is_match(user_id.as_str())
				&& user_id.server_name() == self.sender.server_name())
	}

	/// MSC3905: the `users` regex matches local users only.
	#[inline]
	#[must_use]
	pub fn is_exclusive_user_match(&self, user_id: &UserId) -> bool {
		user_id == self.sender
			|| (self.users.is_exclusive_match(user_id.as_str())
				&& user_id.server_name() == self.sender.server_name())
	}

	/// MSC4502: whether the registration requested `scope`.
	///
	/// The match is exact, against the registration's own scope list.
	#[inline]
	#[must_use]
	pub fn has_scope(&self, scope: &str) -> bool {
		self.registration
			.scopes
			.iter()
			.any(|requested| requested == scope)
	}

	/// Whether the registration names a URL to deliver traffic to.
	///
	/// A registration without one asked for no traffic, so nothing is queued
	/// for it.
	#[inline]
	#[must_use]
	pub fn has_destination(&self) -> bool { destination(&self.registration).is_some() }
}

/// MSC4512: whether the valid proxy claim covers a proxied path.
///
/// `remainder` is the path after its version segments; the claim covers its
/// prefix itself and every path beneath it.
#[implement(RegistrationInfo)]
#[must_use]
pub fn proxy_covers(&self, remainder: &str) -> bool {
	self.proxy_prefix
		.and_then(|prefix| remainder.strip_prefix(prefix))
		.is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// MSC4512: the URL proxied requests are forwarded to.
///
/// Present only with a valid claim, without its trailing slash.
#[implement(RegistrationInfo)]
#[must_use]
pub fn proxy_url(&self) -> Option<&str> {
	self.proxy_prefix
		.and(self.registration.proxy_url.as_deref())
		.map(strip_trailing_slash)
}

/// MSC4512: the registration without its proxy claim.
///
/// The registration's own keys stay as given, so its stored copy is
/// unchanged; only the claim this server honors is cleared.
#[implement(RegistrationInfo)]
#[must_use]
pub fn without_proxy_claim(self) -> Self { Self { proxy_prefix: None, ..self } }

/// MSC4512: the allowed prefix a registration claims, if it claims one.
///
/// Both keys are given or neither. One trailing slash is dropped from each;
/// the prefix must then equal an allowed one, and the URL must be an http or
/// https URL, or a Unix socket where this platform has them.
pub fn proxy_claim(registration: &Registration) -> Result<Option<&'static str>> {
	let (prefix, url) =
		match (registration.proxy_prefix.as_deref(), registration.proxy_url.as_deref()) {
			| (None, None) => return Ok(None),
			| (Some(prefix), Some(url)) =>
				(strip_trailing_slash(prefix), strip_trailing_slash(url)),
			| _ => return Err!("proxy_prefix and proxy_url must be given together"),
		};

	let usable = match socket_path(url)? {
		| Some(_) => cfg!(unix),
		| None => Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https")),
	};

	if !usable {
		return Err!("proxy_url {url:?} is not an http, https or Unix socket URL");
	}

	PROXY_PREFIXES
		.into_iter()
		.find(|allowed| *allowed == prefix)
		.map(Some)
		.ok_or_else(|| err!("proxy_prefix {prefix:?} is not one this server allows"))
}
