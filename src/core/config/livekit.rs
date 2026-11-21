//! Configures the native LiveKit transport for MatrixRTC calls.
//!
//! The section holds what the server needs to reach a LiveKit SFU: its
//! WebSocket URL and an API key pair. The transport is active only when all
//! three are set.

use derive_more::Debug;
use serde::Deserialize;
use smallstr::SmallString;
use tuwunel_macros::config_example_generator;

use crate::{implement, redacted_debug};

// Budgets for typical values: a WebSocket URL of a few dozen bytes, and the
// 15-byte key and 43-byte secret that `livekit-server generate-keys` prints.
type WebSocketUrl = SmallString<[u8; 48]>;
type ApiKey = SmallString<[u8; 16]>;
type ApiSecret = SmallString<[u8; 48]>;

/// LiveKit calls (MSC4195)
///
/// Issues LiveKit access tokens for MatrixRTC calls (Element Call and other
/// MSC4195 clients) from this server, so a LiveKit SFU needs no separate
/// lk-jwt-service. Clients ask this server for a token, and the server
/// creates the call's LiveKit room before answering. LiveKit learns neither
/// Matrix room IDs nor user IDs: room names and participant identities are
/// hashes.
///
/// Set all three of `url`, `api_key` and `api_secret`; with none of them set
/// the native transport is off. The key pair must be one listed in LiveKit's
/// `keys` setting.
///
/// Minimal example:
///
/// ```toml
///     [global.livekit]
///     url = "wss://livekit.example.com"
///     api_key = "APIb3c1e9f07d"
///     api_secret = "replace-with-the-livekit-api-secret"
/// ```
///
/// Tips:
/// - Startup refuses a section with only some of the three set, or a `url`
///   that is not `ws://` or `wss://` or that carries a user name or password.
/// - Changes take a restart; a reload that changes this section is refused.
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.livekit"
)]
pub struct Livekit {
	/// WebSocket URL of the LiveKit SFU, as clients reach it.
	///
	/// Clients receive it as the `m.livekit` transport, and a token request
	/// naming any other URL is refused; the comparison is exact. The server
	/// calls LiveKit's API at the same address over HTTP: `wss` becomes
	/// `https` and `ws` becomes `http`, keeping any path. Unset by default; an
	/// empty value is the same as unset.
	///
	/// reloadable: no
	/// example: "wss://livekit.example.com"
	pub url: Option<WebSocketUrl>,

	/// API key that names this server as the issuer of its LiveKit tokens.
	///
	/// It must be a key in LiveKit's `keys` setting. Unset by default; an
	/// empty value is the same as unset.
	///
	/// reloadable: no
	/// example: "APIb3c1e9f07d"
	pub api_key: Option<ApiKey>,

	/// API secret paired with `api_key` in LiveKit's `keys` setting.
	///
	/// It signs every token this server issues, so keep it private. Unset by
	/// default; an empty value is the same as unset.
	///
	/// display: sensitive
	/// reloadable: no
	#[debug("{}", redacted_debug!(api_secret))]
	pub api_secret: Option<ApiSecret>,
}

/// Whether all three settings are set and not empty.
///
/// Every site that serves or advertises the native transport asks this one
/// predicate. A build without LiveKit support refuses any of the settings at
/// startup, so once the configuration has passed that check, a true answer
/// also means the support is compiled in.
#[implement(Livekit)]
#[inline]
#[must_use]
pub fn is_enabled(&self) -> bool { self.enabled().is_some() }

/// Borrows the URL, API key and API secret when all three are set.
///
/// An empty value counts as unset. Startup refuses a section with only some
/// of them set, so a running server has all three or none.
#[implement(Livekit)]
#[must_use]
pub fn enabled(&self) -> Option<(&str, &str, &str)> {
	let [url, api_key, api_secret] = self.settings();

	Some((url?, api_key?, api_secret?))
}

/// Borrows the URL, API key and API secret, in that order.
///
/// Missing and empty settings both yield `None`.
#[implement(Livekit)]
pub(super) fn settings(&self) -> [Option<&str>; 3] {
	[self.url.as_deref(), self.api_key.as_deref(), self.api_secret.as_deref()]
		.map(|setting| setting.filter(|value| !value.is_empty()))
}
