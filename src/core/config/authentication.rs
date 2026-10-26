//! Configures account authentication and session lifetimes.
//!
//! Login, OIDC, password hashing, and refresh-token policy have dedicated
//! settings. The integration secret authenticates MAS provisioning calls;
//! the emergency password grants access to the server bot account.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures account authentication and session lifetimes.
///
/// Login, OIDC, password hashing, and refresh-token policy have dedicated
/// settings. The integration secret authenticates MAS provisioning calls;
/// the emergency password grants access to the server bot account.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.authentication",
	group = "true",
	ignore = "argon2 login oidc refresh_token"
)]
pub struct Authentication {
	/// Authenticates Matrix Authentication Service provisioning calls with a
	/// shared secret.
	///
	/// When set, the `/_synapse/mas/*` endpoints accept only requests bearing
	/// this exact secret as their bearer token; when unset, they reject every
	/// request. Use a high-entropy value identical to the secret configured in
	/// MAS.
	///
	/// reloadable: yes
	/// example: "kZ2hN5pQ8wXyL4mR7tBfCgJxV3aD6sE1u"
	///
	/// display: sensitive
	/// legacy: mas_secret
	pub mas_secret: Option<String>,

	/// Sets the OpenID token lifetime in seconds.
	///
	/// These tokens primarily serve Matrix account integrations, such as
	/// Vector Integrations in Element, rather than OIDC or OpenID Connect.
	///
	/// reloadable: yes
	/// default: 3600
	/// legacy: openid_token_ttl
	#[serde(default = "default_openid_token_ttl")]
	pub openid_token_ttl: u64,

	/// Sets the access-token lifetime in seconds.
	///
	/// For clients supporting refresh tokens, the access token supplied at
	/// login is invalidated after this interval, soft-logging out the client
	/// until it refreshes.
	///
	/// reloadable: yes
	/// default: 604800
	/// legacy: access_token_ttl
	#[serde(default = "default_access_token_ttl")]
	pub access_token_ttl: u64,

	/// Allows emergency login to the server bot account, currently `@conduit`.
	///
	/// Use it for troubleshooting such as recovering or recreating the admin
	/// room or inviting yourself back. Other recovery methods are described in
	/// the [admin-room troubleshooting
	/// guide](https://tuwunel.chat/troubleshooting.html#lost-access-to-admin-room).
	/// Unsetting this password logs out all sessions for security.
	///
	/// example: "F670$2CP@Hw8mG7RY1$%!#Ic7YA"
	///
	/// display: sensitive
	/// legacy: emergency_password
	pub emergency_password: Option<String>,

	/// Controls password-hashing memory, passes, and lanes.
	///
	/// display: section
	#[serde(default)]
	pub argon2: Argon2,

	/// Controls supported login methods and provider selection.
	///
	/// display: section
	#[serde(default)]
	pub login: Login,

	/// Controls native OIDC authorization and client registration.
	///
	/// display: section
	#[serde(default)]
	pub oidc: Oidc,

	/// Controls refresh-token expiration and reuse.
	///
	/// display: section
	#[serde(default)]
	pub refresh_token: RefreshToken,
}

/// Configures Argon2id password hashing.
///
/// Memory, pass, and lane settings determine new password hashes. Stored
/// hashes retain the parameters used when created.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.authentication.argon2",
	group = "true"
)]
pub struct Argon2 {
	/// Sets the Argon2id working-buffer size for hashing a new password, in 1
	/// KiB blocks.
	///
	/// Each concurrent password operation retains roughly this much memory
	/// for its duration, with no concurrency bound. Stored hashes retain their
	/// parameters until the account's password is next set.
	///
	/// Lowering this weakens resistance to parallel cracking hardware unless
	/// `authentication.argon2.t_cost` compensates; equivalent-strength
	/// (`authentication.argon2.m_cost`, `authentication.argon2.t_cost`) pairs
	/// are (47104, 1), (19456, 2), (12288, 3), (9216, 4), and (7168, 5). The
	/// default and these pairs follow the [OWASP Argon2id
	/// recommendation](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html#argon2id).
	///
	/// reloadable: yes
	/// default: 19456
	/// legacy: argon2_m_cost
	#[serde(default = "default_m_cost")]
	pub m_cost: u32,

	/// Sets the number of Argon2id passes over its working buffer.
	///
	/// Raising this increases CPU cost without changing memory cost. See
	/// `authentication.argon2.m_cost` for equivalent-strength parameter pairs.
	///
	/// reloadable: yes
	/// default: 2
	/// legacy: argon2_t_cost
	#[serde(default = "default_t_cost")]
	pub t_cost: u32,

	/// Sets the number of lanes in an Argon2id working buffer.
	///
	/// Lanes are computed sequentially here, so raising this changes the hash
	/// without adding parallelism. `authentication.argon2.m_cost` must be at
	/// least eight times this value.
	///
	/// reloadable: yes
	/// default: 1
	/// legacy: argon2_p_cost
	#[serde(default = "default_p_cost")]
	pub p_cost: u32,
}

/// Configures client login methods and login tokens.
///
/// Password, token, and existing-session flows have separate enablement
/// controls. SSO presentation settings select how clients show identity
/// providers.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.authentication.login",
	group = "true"
)]
pub struct Login {
	/// Allows an existing session to mint a login token for another client.
	///
	/// This requires interactive authentication and is enabled by default. A
	/// malicious client could use it to spawn additional sessions, so enabling
	/// it has security implications.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: login_via_existing_session
	#[serde(default = "true_fn")]
	pub via_existing_session: bool,

	/// Enables the login-token route to accept tokens.
	///
	/// The server can generate tokens for authorization flows such as SSO,
	/// which disabling this may break. This differs from
	/// `authentication.login.via_existing_session` and lacks that setting's
	/// security implications: keep this enabled while disabling the other to
	/// prevent clients from commanding token creation without preventing the
	/// server from creating tokens.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: login_via_token
	#[serde(default = "true_fn")]
	pub via_token: bool,

	/// Enables traditional username-and-password login.
	///
	/// Disable this to allow only other mechanisms, such as SSO.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: login_with_password
	#[serde(default = "true_fn")]
	pub with_password: bool,

	/// Sets the login-token lifetime in milliseconds.
	///
	/// These short-lived tokens serve `m.login.token`, allowing existing
	/// sessions to create new sessions. See
	/// `authentication.login.via_existing_session`.
	///
	/// reloadable: yes
	/// default: 120000
	/// legacy: login_token_ttl
	#[serde(default = "default_token_ttl")]
	pub token_ttl: u64,

	/// Experimentally replaces the client login provider list with one "Sign
	/// in with single sign-on" button.
	///
	/// The button links to `/_matrix/client/v3/login/sso/redirect`, attempting
	/// every configured provider for the same Matrix user; every authorization
	/// must succeed because providers cannot reliably be skipped. Disabled by
	/// default, it otherwise lets clients list providers so privacy-conscious
	/// users can authorize only their choice. For clients that already show
	/// one button, use the provider configuration's `default` option instead.
	///
	/// reloadable: yes
	/// legacy: single_sso
	#[serde(default)]
	pub single_sso: bool,

	/// Replaces the client login provider list with one button for a custom
	/// provider page.
	///
	/// The "Sign in with single sign-on" button links to
	/// `/_matrix/client/v3/login/sso/redirect`, which the deployment's reverse
	/// proxy must intercept to serve a custom list. Each entry links or
	/// redirects to its configured provider at
	/// `/_matrix/client/v3/login/sso/redirect/<client_id>`. The default false
	/// lets clients generate their provider list or hide SSO options when none
	/// are configured.
	///
	/// reloadable: yes
	/// legacy: sso_custom_providers_page
	#[serde(default)]
	pub sso_custom_providers_page: bool,

	/// Prefers OIDC-aware authentication under
	/// [MSC3824](https://github.com/matrix-org/matrix-spec-proposals/pull/3824).
	///
	/// When clients find `oauth_aware_preferred` true and support that
	/// authentication type, they should present it as the only login or
	/// registration method.
	///
	/// reloadable: yes
	/// legacy: oidc_aware_preferred sso_aware_preferred
	#[serde(default, alias = "sso_aware_preferred")]
	pub oidc_aware_preferred: bool,
}

/// Configures the built-in OpenID Connect provider.
///
/// Native authentication, client registration, and authorization policy
/// have separate controls. Per-IP rate limits bound requests to its
/// endpoints.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.authentication.oidc",
	group = "true"
)]
pub struct Oidc {
	/// Enables native registration and login on the built-in OIDC provider.
	///
	/// This authenticates next-generation Matrix clients against local
	/// accounts without a third-party `identity_provider`; when false, as by
	/// default, the provider only brokers authentication through a configured
	/// upstream provider. When true, requests selecting no provider receive a
	/// login or registration page and require `well_known.client`; the login
	/// page offers local accounts and configured providers, with one SSO entry
	/// under `authentication.login.single_sso` or
	/// `authentication.login.sso_custom_providers_page`. An explicit `idp_id`
	/// goes directly to its provider. Registration honors
	/// `registration.enable`, the registration token, and root
	/// `registration_terms` exactly as client registration does.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: oidc_native_auth
	#[serde(default)]
	pub native_auth: bool,

	/// Requires OIDC clients to request an
	/// [MSC2967](https://github.com/matrix-org/matrix-spec-proposals/pull/2967)
	/// device scope.
	///
	/// When false, clients omitting `urn:matrix:client:device:<id>` receive a
	/// server-generated device ID echoed in the granted scope. When true,
	/// authorization-code grants without a device scope are rejected,
	/// following the proposal's expectation that clients own their device IDs.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: oidc_require_device_scope
	#[serde(default)]
	pub require_device_scope: bool,

	/// Requires [RFC 7636](https://www.rfc-editor.org/rfc/rfc7636) PKCE with
	/// S256 on OIDC authorization-code grants.
	///
	/// When true, the authorize endpoint rejects requests without
	/// `code_challenge`, as
	/// [MSC2964](https://github.com/matrix-org/matrix-spec-proposals/pull/2964)
	/// mandates for public clients. A supplied challenge must always use S256,
	/// and `plain` is rejected regardless of this setting. Disable only
	/// temporarily for a legacy client unable to send a challenge.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: oidc_require_pkce
	#[serde(default = "true_fn")]
	pub require_pkce: bool,

	/// Rejects OIDC authorization-code grants containing unrecognized scopes
	/// instead of narrowing the grant.
	///
	/// When false, as by default, unknown scope tokens are dropped and the
	/// narrowed `scope` is echoed to the client under [RFC
	/// 6749](https://www.rfc-editor.org/rfc/rfc6749); when true, an unknown
	/// scope is rejected. `openid` and both spellings of the
	/// [MSC2967](https://github.com/matrix-org/matrix-spec-proposals/pull/2967)
	/// device and API scopes are always recognized.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: oidc_strict_scope
	#[serde(default)]
	pub strict_scope: bool,

	/// Requires an initial access token for dynamic OIDC client registration
	/// under [RFC 7591](https://www.rfc-editor.org/rfc/rfc7591).
	///
	/// When set, callers must supply it as an `Authorization: Bearer`
	/// credential; clients that omit it cannot dynamically register for
	/// next-generation login. Leave empty unless every OAuth client is
	/// registered out of band.
	///
	/// display: sensitive
	/// reloadable: yes
	/// default:
	/// legacy: oidc_registration_access_token
	#[serde(default)]
	pub registration_access_token: String,

	/// Lists permitted hosts and private-use schemes for dynamically
	/// registered OIDC redirect URIs.
	///
	/// When nonempty, every redirect URI must match or registration is
	/// rejected: hosts match case-insensitively, while hostless private-use
	/// URIs such as `io.element.android:/` match their scheme under [RFC
	/// 8252](https://www.rfc-editor.org/rfc/rfc8252). For example,
	/// `["element.io", "io.element.android"]` covers web and mobile clients.
	///
	/// The empty default imposes no registration restriction and vouches for
	/// no client, so every sign-in remains subject to
	/// `authentication.oidc.require_client_approval`. Listing an entry vouches
	/// for any responding client: a scheme covers any device app claiming it
	/// under [RFC 8252 section
	/// 8.6](https://www.rfc-editor.org/rfc/rfc8252#section-8.6), and a host
	/// covers every path.
	///
	/// reloadable: yes
	/// default: []
	/// legacy: oidc_registration_allowed_redirect_hosts
	#[serde(default)]
	pub registration_allowed_redirect_hosts: Vec<String>,

	/// Asks users to approve unvetted OIDC clients before issuing
	/// authorization codes.
	///
	/// Dynamic registration is open by default, so anyone can register a
	/// controlled redirect target and send users an authorization link;
	/// without confirmation, one click gives that client an account
	/// authorization code. When true, as by default, unvetted sign-ins first
	/// show an approve/deny page naming the client. A host or private-use
	/// scheme listed in
	/// `authentication.oidc.registration_allowed_redirect_hosts` vets its
	/// client and restores prompt-free sign-in. When showing this prompt,
	/// allow enough `authentication.login.token_ttl` for users to read it.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: oidc_require_client_approval
	#[serde(default = "true_fn")]
	pub require_client_approval: bool,

	/// Requires `client_uri` in dynamic registration under [RFC
	/// 7591](https://www.rfc-editor.org/rfc/rfc7591) and
	/// [MSC2966](https://github.com/matrix-org/matrix-spec-proposals/pull/2966).
	///
	/// When false, as by default, it is optional; any supplied URI still
	/// requires HTTPS, a host, and no userinfo, and other request URLs must
	/// share that host or a subdomain. When true, registrations lacking an
	/// HTTPS `client_uri` fail with `invalid_client_metadata`, enforcing the
	/// common-base model for every client.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: oidc_registration_require_client_uri
	#[serde(default)]
	pub registration_require_client_uri: bool,

	/// Sets the per-client-IP token-bucket refill rate for OIDC endpoints in
	/// requests per second.
	///
	/// The throttle is shared across authorize, token, dynamic-registration,
	/// and device-grant endpoints. The default 0 disables it and preserves
	/// open access; raise this with `authentication.oidc.rc_burst_count` to
	/// protect an exposed server. Because the key is client IP, rates that
	/// limit brute force can also affect many users behind one NAT, so size
	/// the burst accordingly.
	///
	/// reloadable: yes
	/// default: 0
	/// legacy: oidc_rc_per_second
	#[serde(default)]
	pub rc_per_second: u32,

	/// Sets the token-bucket depth for the OIDC endpoint throttle.
	///
	/// This is the requests one client IP can make in a burst before
	/// `authentication.oidc.rc_per_second` governs replenishment. It is
	/// ignored when that refill rate is 0.
	///
	/// reloadable: yes
	/// default: 0
	/// legacy: oidc_rc_burst_count
	#[serde(default)]
	pub rc_burst_count: u32,
}

/// Configures refresh-token expiry and reuse handling.
///
/// Lifetime policy selects idle or absolute deadlines and soft or hard
/// logout. Reuse controls distinguish a grace window from suspected
/// compromise.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.authentication.refresh_token",
	group = "true"
)]
pub struct RefreshToken {
	/// Sets the refresh-token lifetime in seconds.
	///
	/// Tokens are rejected after this interval;
	/// `authentication.refresh_token.idle_only` determines whether the
	/// deadline slides with use or stays fixed at issuance. The default 0
	/// disables refresh-token expiry entirely. A typical enabled value is
	/// 259200, or three days.
	///
	/// reloadable: yes
	/// default: 0
	/// legacy: refresh_token_ttl
	#[serde(default)]
	pub ttl: u64,

	/// Uses the refresh-token lifetime as an idle timeout instead of an
	/// absolute session lifetime.
	///
	/// With a nonzero `authentication.refresh_token.ttl`, true, the default,
	/// resets the deadline to now plus that lifetime after each successful
	/// rotation; false retains an existing deadline rather than extending it. A
	/// successful grace replay retains its current successor and does not
	/// reset that successor's deadline. A zero TTL issues replacement tokens
	/// without expiry.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: refresh_token_idle_only
	#[serde(default = "true_fn")]
	pub idle_only: bool,

	/// Removes the device on refresh-token expiry instead of soft-logging out.
	///
	/// When false, as by default, expiry returns `M_UNKNOWN_TOKEN` with
	/// `soft_logout: true`, allowing the client to retain E2EE keys and local
	/// state and reauthenticate on the same device. When true, expiry
	/// invalidates the access token, deletes the device record, and signals
	/// `soft_logout: false`; the next session uses a new device and cannot
	/// recover E2EE history from local state alone, trading usability for the
	/// [CWE-613](https://cwe.mitre.org/data/definitions/613.html) guarantee.
	/// Tokens issued with zero TTL do not expire, and changing the current TTL
	/// does not remove an already stored expiry.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: refresh_token_hard_logout
	#[serde(default)]
	pub hard_logout: bool,

	/// Sets the grace window in seconds for a benign refresh-token double
	/// submission.
	///
	/// After rotation, the spent token is retained for one generation; within
	/// a nonzero window, a replay can issue a fresh access token while
	/// retaining its current, resolvable, unexpired successor. An expired
	/// successor triggers the configured expiry logout, while an unresolvable
	/// successor is refused without changes.
	///
	/// A token classified as spent outside grace on arrival is rejected, with
	/// device revocation only when `authentication.refresh_token.reuse_revoke`
	/// is enabled; zero disables grace. A request current on arrival but spent
	/// while waiting for the device lock can instead be refused without
	/// revocation, including at zero grace.
	///
	/// reloadable: yes
	/// default: 15
	/// legacy: refresh_token_reuse_grace
	#[serde(default = "default_reuse_grace")]
	pub reuse_grace: u64,

	/// Revokes the device when refresh-token reuse is detected.
	///
	/// When true, as by default, reuse detected outside
	/// `authentication.refresh_token.reuse_grace` on arrival removes the
	/// device under the
	/// [RFC 6819](https://www.rfc-editor.org/rfc/rfc6819) compromised-session
	/// model. When false, the replayed request is rejected but the device
	/// remains intact, a less restrictive behavior operators using another
	/// OAuth client may prefer.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: refresh_token_reuse_revoke
	#[serde(default = "true_fn")]
	pub reuse_revoke: bool,
}

impl Default for Authentication {
	fn default() -> Self {
		Self {
			mas_secret: None,
			openid_token_ttl: default_openid_token_ttl(),
			access_token_ttl: default_access_token_ttl(),
			emergency_password: None,
			argon2: Argon2::default(),
			login: Login::default(),
			oidc: Oidc::default(),
			refresh_token: RefreshToken::default(),
		}
	}
}

impl Default for Argon2 {
	fn default() -> Self {
		Self {
			m_cost: default_m_cost(),
			t_cost: default_t_cost(),
			p_cost: default_p_cost(),
		}
	}
}

impl Default for Login {
	fn default() -> Self {
		Self {
			via_existing_session: true_fn(),
			via_token: true_fn(),
			with_password: true_fn(),
			token_ttl: default_token_ttl(),
			single_sso: Default::default(),
			sso_custom_providers_page: Default::default(),
			oidc_aware_preferred: Default::default(),
		}
	}
}

impl Default for Oidc {
	fn default() -> Self {
		Self {
			native_auth: Default::default(),
			require_device_scope: Default::default(),
			require_pkce: true_fn(),
			strict_scope: Default::default(),
			registration_access_token: Default::default(),
			registration_allowed_redirect_hosts: Default::default(),
			require_client_approval: true_fn(),
			registration_require_client_uri: Default::default(),
			rc_per_second: Default::default(),
			rc_burst_count: Default::default(),
		}
	}
}

impl Default for RefreshToken {
	fn default() -> Self {
		Self {
			ttl: Default::default(),
			idle_only: true_fn(),
			hard_logout: Default::default(),
			reuse_grace: default_reuse_grace(),
			reuse_revoke: true_fn(),
		}
	}
}

fn default_openid_token_ttl() -> u64 { 60 * 60 }

fn default_access_token_ttl() -> u64 { 604_800 }

fn default_m_cost() -> u32 { 19 * 1024 }

fn default_t_cost() -> u32 { 2 }

fn default_p_cost() -> u32 { 1 }

fn default_token_ttl() -> u64 { 2 * 60 * 1000 }

fn default_reuse_grace() -> u64 { 15 }
