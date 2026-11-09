//! Configures TURN relay credentials and servers.
//!
//! Shared secrets generate credentials, with static credentials and external
//! credential providers available as alternatives. URI, lifetime, and
//! guest-access settings govern client use.

use serde::Deserialize;
use smallstr::SmallString;
use tuwunel_macros::config_example_generator;
use url::Url;

use super::PathBuf;
use crate::implement;

type KeyId = SmallString<[u8; 32]>;

/// TURN relay for calls
///
/// Tells clients which TURN servers relay their legacy Matrix calls, and
/// with what credentials, through `/_matrix/client/v3/voip/turnServer`.
/// No TURN server is built in; the guide at
/// <https://tuwunel.chat/calls/turn.html> uses coturn. With `uris` empty
/// and no provider configured (the default), `turnServer` answers 404.
///
/// Pick one credential source:
///
/// - Shared secret (recommended for your own TURN server): `secret` or
///   `secret_file`, equal to coturn's `static-auth-secret`. Each request
///   gets a fresh username `<expiry>:<user_id>` and HMAC-SHA1 password,
///   valid for `ttl` seconds.
/// - Static: `username` and `password`, sent to every client as they are;
///   ignored while a secret is set.
/// - Provider: `[global.turn.cloudflare]` for Cloudflare's hosted TURN, or
///   `[global.turn.endpoint]` for any HTTP endpoint that issues expiring
///   credentials. The server URIs come from the provider too, so `uris`,
///   the secret and the static credentials are ignored.
///
/// With `uris` set and no source, startup logs a warning and clients
/// receive an empty username and password.
///
/// Minimal example:
///
/// ```toml
///     [global.turn]
///     uris = ["turn:turn.example.com?transport=udp"]
///     secret_file = "/etc/tuwunel/.turn_secret"
/// ```
///
/// Tips:
/// - `secret_file` is read on every request, so rotating the secret needs
///   no reload or restart.
/// - Guests get 403 from `turnServer` unless `allow_guests` is on.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
// No Debug: secrets are reachable from this section.
#[derive(Clone, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.turn",
	group = "true",
	ignore = "endpoint cloudflare"
)]
pub struct Turn {
	/// Allows guest users to access TURN credentials.
	///
	/// This is equivalent to Synapse's `turn_allow_guests` setting and permits
	/// guests to call `/_matrix/client/v3/voip/turnServer`.
	///
	/// reloadable: yes
	/// legacy: turn_allow_guests
	#[serde(default)]
	pub allow_guests: bool,

	/// Supplies a static TURN username when the recommended shared secret
	/// `turn.secret` is not used.
	///
	/// reloadable: yes
	/// legacy: turn_username
	#[serde(default)]
	pub username: String,

	/// Supplies a static TURN password when the recommended shared secret
	/// `turn.secret` is not used.
	///
	/// display: sensitive
	/// reloadable: yes
	/// legacy: turn_password
	#[serde(default)]
	pub password: String,

	#[expect(clippy::doc_link_with_quotes)]
	/// Lists TURN server URIs.
	///
	/// Replace "example.turn.uri" with the TURN domain, such as coturn's
	/// `realm` setting. For TURN over TLS, replace the `turn:` prefix with
	/// `turns:`.
	///
	/// reloadable: yes
	/// example: ["turn:example.turn.uri?transport=udp",
	/// "turn:example.turn.uri?transport=tcp"]
	///
	/// default: []
	/// legacy: turn_uris
	#[serde(default)]
	pub uris: Vec<String>,

	/// Supplies the TURN shared secret for HMAC-SHA1 credential generation,
	/// which is more secure than the traditional static username-and-password
	/// credentials that remain available when needed.
	///
	/// display: sensitive
	/// reloadable: yes
	/// legacy: turn_secret
	#[serde(default)]
	pub secret: Option<String>,

	/// Reads the TURN shared secret from a file.
	///
	/// This takes precedence over `turn.secret` and falls back to it when the
	/// file cannot be opened. Surrounding whitespace is trimmed so a trailing
	/// newline is not part of the secret. A present but blank file resolves to
	/// no secret without falling back.
	///
	/// reloadable: yes
	/// example: "/etc/tuwunel/.turn_secret"
	/// legacy: turn_secret_file
	pub secret_file: Option<PathBuf>,

	/// Sets the TURN lifetime in seconds.
	///
	/// reloadable: yes
	/// default: 86400
	/// legacy: turn_ttl
	#[serde(default = "default_ttl")]
	pub ttl: u64,

	/// Settings for an HTTP endpoint issuing TURN credentials.
	///
	/// display: section
	#[serde(default)]
	pub endpoint: Endpoint,

	/// Settings for Cloudflare's hosted TURN service.
	///
	/// display: section
	#[serde(default)]
	pub cloudflare: Cloudflare,
}

/// TURN credentials from an HTTP endpoint
///
/// Fetches TURN credentials and server URIs from an HTTP endpoint, for a
/// TURN service that issues expiring credentials through an API rather
/// than sharing a secret. Tuwunel sends `GET <url>` with `service=turn`
/// and `ttl=<turn.ttl>` added to the query, and accepts either answer:
///
/// - TURN REST API: `{"username", "password", "ttl", "uris"}`, as the
///   STUNner auth service returns from `/`.
/// - WebRTC ICE servers: `{"iceServers": [...]}` or a bare list, each
///   entry holding `urls`, `username` and `credential`, as STUNner's
///   `/ice` returns. Clients take one username and password, so entries
///   whose credentials differ from the first entry's are dropped.
///
/// Only `turn:` and `turns:` URIs reach clients, as the Matrix spec
/// requires; any `stun:` URIs in the answer are left out.
///
/// A service with another request or answer shape needs a small adapter
/// in front of it. While `url` is set, `turn.uris`, the shared secret and
/// the static credentials are ignored.
///
/// ```toml
///     [global.turn.endpoint]
///     url = "http://127.0.0.1:8088/"
///     token_file = "/etc/tuwunel/.turn_endpoint_token"
/// ```
///
/// Tips:
/// - Query parameters already in `url`, other than `service` and `ttl`,
///   are kept, for a service that takes its API key in the query.
/// - One set of credentials is shared by every user and renewed after
///   half its lifetime, so the endpoint sees about two requests per
///   credential lifetime however many clients ask. No Matrix user ID is
///   sent.
/// - After a failed request the endpoint is not asked again for a minute;
///   meanwhile clients get whatever credentials are still unexpired, or
///   502.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
// No Debug: secrets are reachable from this section.
#[derive(Clone, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.turn.endpoint",
	group = "true"
)]
pub struct Endpoint {
	/// URL of the credential endpoint.
	///
	/// display: sensitive
	/// reloadable: yes
	/// example: "http://127.0.0.1:8088/"
	pub url: Option<Url>,

	/// Supplies a bearer token sent in the `Authorization` header.
	///
	/// display: sensitive
	/// reloadable: yes
	pub token: Option<String>,

	/// Reads the bearer token from a file.
	///
	/// This takes precedence over `token` and falls back to it when the file
	/// cannot be opened. Surrounding whitespace is trimmed.
	///
	/// reloadable: yes
	/// example: "/etc/tuwunel/.turn_endpoint_token"
	pub token_file: Option<PathBuf>,
}

/// Cloudflare Realtime TURN
///
/// Fetches expiring TURN credentials and server URIs from Cloudflare's
/// hosted TURN service. Create a TURN key in the Cloudflare dashboard and
/// set its key ID and API token here. Cloudflare credentials live at most
/// 48 hours, so a longer `turn.ttl` is shortened to that. Only its TURN
/// URIs reach clients, without the port 53 ones browsers block.
///
/// While `key_id` is set, `turn.uris`, the shared secret and the static
/// credentials are ignored. Set this section or `[global.turn.endpoint]`,
/// not both.
///
/// ```toml
///     [global.turn.cloudflare]
///     key_id = "0123456789abcdef0123456789abcdef"
///     token_file = "/etc/tuwunel/.cloudflare_turn_token"
/// ```
///
/// Tips:
/// - One set of credentials is shared by every user and renewed after
///   half its lifetime, so Cloudflare sees about two requests per
///   credential lifetime however many clients ask. No Matrix user ID is
///   sent.
/// - `token_file` is read at each renewal, so rotating the token needs no
///   reload or restart.
/// - After a failed request Cloudflare is not asked again for a minute;
///   meanwhile clients get whatever credentials are still unexpired, or
///   502.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
// No Debug: secrets are reachable from this section.
#[derive(Clone, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.turn.cloudflare",
	group = "true"
)]
pub struct Cloudflare {
	/// ID of the Cloudflare TURN key; setting it selects this provider.
	///
	/// reloadable: yes
	/// default: ""
	#[serde(default)]
	pub key_id: KeyId,

	/// API token of the Cloudflare TURN key.
	///
	/// display: sensitive
	/// reloadable: yes
	pub token: Option<String>,

	/// Reads the API token from a file.
	///
	/// This takes precedence over `token` and falls back to it when the file
	/// cannot be opened. Surrounding whitespace is trimmed.
	///
	/// reloadable: yes
	/// example: "/etc/tuwunel/.cloudflare_turn_token"
	pub token_file: Option<PathBuf>,
}

/// External source of TURN credentials, when one is configured.
///
/// Each variant borrows its section together with the setting that selects
/// it, so a caller need not check that setting again.
#[derive(Clone, Copy)]
pub enum Provider<'a> {
	/// `[global.turn.endpoint]` and its URL.
	Endpoint(&'a Endpoint, &'a Url),

	/// `[global.turn.cloudflare]` and its TURN key ID.
	Cloudflare(&'a Cloudflare, &'a str),
}

impl Default for Turn {
	fn default() -> Self {
		Self {
			allow_guests: Default::default(),
			username: Default::default(),
			password: Default::default(),
			uris: Default::default(),
			secret: Default::default(),
			secret_file: None,
			ttl: default_ttl(),
			endpoint: Default::default(),
			cloudflare: Default::default(),
		}
	}
}

/// Selects the configured credential provider.
///
/// `[global.turn.endpoint]` wins when both provider sections are set,
/// though startup rejects that configuration.
#[implement(Turn)]
#[must_use]
pub fn provider(&self) -> Option<Provider<'_>> { self.providers().next() }

/// Lists every configured credential provider, the endpoint first.
///
/// More than one is a configuration startup rejects, so every caller but
/// that check wants [`Turn::provider`].
#[implement(Turn)]
pub fn providers(&self) -> impl Iterator<Item = Provider<'_>> {
	let endpoint = self
		.endpoint
		.url
		.as_ref()
		.map(|url| Provider::Endpoint(&self.endpoint, url));

	let cloudflare = Some(self.cloudflare.key_id.as_str())
		.filter(|key_id| !key_id.is_empty())
		.map(|key_id| Provider::Cloudflare(&self.cloudflare, key_id));

	endpoint.into_iter().chain(cloudflare)
}

fn default_ttl() -> u64 { 60 * 60 * 24 }
