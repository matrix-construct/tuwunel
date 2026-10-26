//! Loads and validates server configuration.
//!
//! Configuration types preserve startup sources for reloads and expose typed
//! settings to the rest of the workspace. Field documentation also supplies the
//! generated example configuration.

pub mod cache;
pub mod check;
pub mod database;
pub mod federation;
mod format;
pub mod http;
mod identity_provider_serde;
pub mod ip_source;
mod jwt;
pub mod manager;
pub mod media;
mod net;
pub mod network;
mod normalize;
pub mod proxy;
mod regenerate;
mod rocksdb;
pub mod room_version;
pub mod rooms;
pub mod runtime;
pub mod sending;
pub mod sources;
#[cfg(test)]
mod tests;
pub mod well_known;

use std::{
	collections::{BTreeMap, BTreeSet},
	path::{Path, PathBuf},
};

use bytesize::ByteSize;
use derive_more::Debug;
pub use figment::{Figment, value::Value as FigmentValue};
use figment::{
	Profile, Provider,
	providers::{Env, Format},
};
use itertools::Itertools;
use regex::RegexSet;
pub use rocksdb::Rocksdb;
use ruma::{
	OwnedMxcUri, OwnedRoomOrAliasId, OwnedServerName, OwnedUserId,
	api::client::discovery::discover_support::ContactRole,
};
use serde::{Deserialize, de::IgnoredAny};
use smallstr::SmallString;
use tuwunel_macros::config_example_generator;
use url::Url;

pub use self::{
	cache::Cache,
	check::check,
	database::Database,
	federation::Federation,
	http::Http,
	ip_source::IpSource,
	manager::Manager,
	media::Media,
	network::Network,
	regenerate::{
		Overwrite, RegenerateOptions, RegenerationSummary, example_config, regenerate_config,
		write_example_config,
	},
	rooms::Rooms,
	runtime::Runtime,
	sending::Sending,
	sources::Sources,
};
use self::{
	format::Toml,
	normalize::{
		assert_canonical, environment, normalize as normalize_provider, redact_error, unknown,
	},
	proxy::ProxyConfig,
};
use crate::{
	Err, Result, err, implement, redacted_debug,
	utils::{
		self,
		bytes::{deserialize_bytesize_u64, deserialize_bytesize_usize},
		hash::Cost,
		sys,
	},
};

// Later prefixes override earlier ones.
pub(crate) const ENV_PREFIXES: [&str; 3] = ["CONDUIT_", "CONDUWUIT_", "TUWUNEL_"];

/// Stores the configured localpart for the server's administrative user.
///
/// The inline budget covers ordinary Matrix localparts without a heap
/// allocation. Longer valid localparts spill transparently.
pub type ServerUserLocalpart = SmallString<[u8; 32]>;

/// Stores an object-storage bucket name inline in the common case.
pub type StorageBucketName = SmallString<[u8; 64]>;

/// All the config options for tuwunel.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global",
	undocumented = "# This item is undocumented. Please contribute documentation for it.",
	header = r#"### Tuwunel Configuration
###
### THIS FILE IS GENERATED. CHANGES/CONTRIBUTIONS IN THE REPO WILL BE
### OVERWRITTEN!
###
### You should rename this file before configuring your server. Changes to
### documentation and defaults can be contributed in source code at
### src/core/config/. This file is generated when building.
###
### Any values pre-populated are the default values for said config option.
###
### At the minimum, you MUST edit all the config options to your environment
### that say "YOU NEED TO EDIT THIS".
###
### For more information, see:
### https://tuwunel.chat/configuration.html
"#,
	ignore = "rocksdb catchall well_known tls rate_limiting ldap jwt appservice \
	          identity_provider storage_provider registration_terms smtp cache database runtime \
	          http network media federation rooms sending",
	forbidden = "database_restore_backup force_migration"
)]
pub struct Config {
	/// RocksDB engine settings.
	///
	/// display: section
	#[serde(default)]
	pub rocksdb: Rocksdb,

	/// The server_name is the pretty name of this server. It is used as a
	/// suffix for user and room IDs/aliases.
	///
	/// See the docs for reverse proxying and delegation:
	/// https://tuwunel.chat/deploying/generic.html#setting-up-the-reverse-proxy
	///
	/// Also see the `[global.well_known]` config section at the very bottom.
	///
	/// Examples of delegation:
	/// - https://matrix.org/.well-known/matrix/server
	/// - https://matrix.org/.well-known/matrix/client
	///
	/// YOU NEED TO EDIT THIS. THIS CANNOT BE CHANGED AFTER WITHOUT A DATABASE
	/// WIPE.
	///
	/// example: "girlboss.ceo"
	#[cfg_attr(test, serde(default = "default_server_name"))]
	pub server_name: OwnedServerName,

	/// This is the only directory where tuwunel will save its data, including
	/// media. Note: this was previously "/var/lib/matrix-conduit".
	///
	/// default: "/var/lib/tuwunel"
	#[serde(default = "default_database_path")]
	pub database_path: PathBuf,

	/// Text which will be added to the end of the user's displayname upon
	/// registration with a space before the text. In Conduit, this was the
	/// lightning bolt emoji.
	///
	/// To disable, set this to "" (an empty string).
	///
	/// reloadable: yes
	/// default: "💕"
	#[serde(default = "default_new_user_displayname_suffix")]
	pub new_user_displayname_suffix: String,

	/// Configures direct TLS listeners.
	///
	/// Values are read from the separate `[global.tls]` section. Certificate
	/// and key paths must be supplied together before TLS is enabled.
	// external structure; separate section
	#[serde(default)]
	pub tls: TlsConfig,

	/// Error on startup if any config option specified is unknown to Tuwunel.
	///
	/// This is false by default to allow easier deprecation or removal of
	/// config options in the future without breaking existing deployments. The
	/// default behaviour is to simply warn on startup.
	/// reloadable: yes
	#[serde(default)]
	pub error_on_unknown_config_opts: bool,

	/// Restore this online database backup on startup, before the database is
	/// opened. The value is a backup ID as listed by `!admin server
	/// list-backups`, or 0 for the most recent backup. Set by the
	/// `--restore-backup` command line argument, and refused from a
	/// configuration file, where it would repeat the restore on every
	/// startup.
	pub database_restore_backup: Option<u32>,

	/// Minimum timeout a client can request for long-polling sync. Requests
	/// will be clamped up to this value if smaller.
	///
	/// reloadable: yes
	/// default: 5000
	#[serde(default = "default_client_sync_timeout_min")]
	pub client_sync_timeout_min: u64,

	/// Default timeout for long-polling sync if a client does not request
	/// another in their query-string.
	///
	/// reloadable: yes
	/// default: 30000
	#[serde(default = "default_client_sync_timeout_default")]
	pub client_sync_timeout_default: u64,

	/// Maximum timeout a client can request for long-polling sync. Requests
	/// will be clamped down to this value if larger.
	///
	/// reloadable: yes
	/// default: 90000
	#[serde(default = "default_client_sync_timeout_max")]
	pub client_sync_timeout_max: u64,

	/// Appservice URL read and total timeout, defaulting to 35 seconds as
	/// generally appservices are hosted within the same network.
	///
	/// default: 35
	#[serde(default = "default_appservice_timeout")]
	pub appservice_timeout: u64,

	/// Appservice URL idle connection pool timeout (seconds).
	///
	/// default: 300
	#[serde(default = "default_appservice_idle_timeout")]
	pub appservice_idle_timeout: u64,

	/// Notification gateway pusher idle connection pool timeout.
	///
	/// default: 15
	#[serde(default = "default_pusher_idle_timeout")]
	pub pusher_idle_timeout: u64,

	/// Enables registration. If set to false, no users can register on this
	/// server.
	///
	/// If set to true without a token configured, users can register with no
	/// form of 2nd-step only if you set the following option to true:
	/// `yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse`
	///
	/// If you would like registration only via token reg, please configure
	/// `registration_token` or `registration_token_file`.
	/// reloadable: yes
	#[serde(default)]
	pub allow_registration: bool,

	/// Enabling this setting opens registration to anyone without restrictions.
	/// This makes your server vulnerable to abuse
	/// reloadable: yes
	#[serde(default)]
	pub yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse: bool,

	/// A static registration token that new users will have to provide when
	/// creating an account. If unset and `allow_registration` is true,
	/// you must set
	/// `yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse`
	/// to true to allow open registration without any conditions.
	///
	/// YOU NEED TO EDIT THIS OR USE registration_token_file.
	///
	/// reloadable: yes
	/// example: "o&^uCtes4HPf0Vu@F20jQeeWE7"
	///
	/// display: sensitive
	pub registration_token: Option<String>,

	/// Path to a file on the system that gets read for additional registration
	/// tokens. Multiple tokens can be added if you separate them with
	/// whitespace
	///
	/// tuwunel must be able to access the file, and it must not be empty
	///
	/// reloadable: yes
	/// example: "/etc/tuwunel/.reg_token"
	pub registration_token_file: Option<PathBuf>,

	/// A pre-shared secret enabling out-of-band account creation via the
	/// Synapse-style `/_synapse/admin/v1/register` endpoint. The endpoint is
	/// only available when this is set. Requests authenticate by HMAC-SHA1
	/// keyed on this value; UIAA is bypassed.
	///
	/// Use a high-entropy value (at least 32 bytes) and treat it as a
	/// secret of equivalent power to a server admin's access token.
	///
	/// reloadable: yes
	/// example: "kZ2hN5pQ8wXyL4mR7tBfCgJxV3aD6sE1u"
	///
	/// display: sensitive
	pub registration_shared_secret: Option<String>,

	/// Path to a file containing the registration shared secret. Takes
	/// precedence over `registration_shared_secret`, and falls back to it when
	/// the file cannot be opened. Surrounding whitespace is trimmed off, so a
	/// trailing newline does not become part of the secret. A file which is
	/// present but blank resolves to no secret rather than falling back.
	///
	/// reloadable: yes
	/// example: "/etc/tuwunel/.reg_shared_secret"
	pub registration_shared_secret_file: Option<PathBuf>,

	/// Shared secret the Matrix Authentication Service (MAS) authenticates its
	/// provisioning calls with. When set, the `/_synapse/mas/*` endpoints
	/// accept only requests bearing this exact secret as their bearer token,
	/// rejecting all others; when unset, those endpoints reject every request.
	///
	/// Use a high-entropy value and keep it identical to the secret configured
	/// on the MAS side.
	///
	/// reloadable: yes
	/// example: "kZ2hN5pQ8wXyL4mR7tBfCgJxV3aD6sE1u"
	///
	/// display: sensitive
	pub mas_secret: Option<String>,

	/// Size of the Argon2id working buffer used to hash a new password, in 1
	/// KiB blocks.
	///
	/// Each hash holds the buffer for its duration, so peak memory rises by
	/// roughly this much for every password operation in flight, with no bound
	/// on how many run at once. Stored hashes carry the values they were made
	/// with, so a change reaches an account only when its password is next set.
	///
	/// Lowering this weakens the hash against parallel cracking hardware unless
	/// `argon2_t_cost` rises to compensate; the equivalent-strength pairs of
	/// (`argon2_m_cost`, `argon2_t_cost`) are (47104, 1), (19456, 2), (12288,
	/// 3), (9216, 4) and (7168, 5). The default and those pairs are the OWASP
	/// Argon2id recommendation:
	/// <https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html#argon2id>
	///
	/// reloadable: yes
	///
	/// default: 19456
	#[serde(default = "default_argon2_m_cost")]
	pub argon2_m_cost: u32,

	/// Number of passes an Argon2id hash makes over its working buffer.
	///
	/// Raising it scales the CPU cost of a hash without changing its memory
	/// cost. See `argon2_m_cost` for the equivalent-strength pairs.
	///
	/// reloadable: yes
	///
	/// default: 2
	#[serde(default = "default_argon2_t_cost")]
	pub argon2_t_cost: u32,

	/// Number of lanes an Argon2id working buffer is divided into.
	///
	/// Lanes are computed sequentially here, so raising this changes the hash
	/// without buying any parallelism. `argon2_m_cost` must be at least eight
	/// times this value.
	///
	/// reloadable: yes
	///
	/// default: 1
	#[serde(default = "default_argon2_p_cost")]
	pub argon2_p_cost: u32,

	/// Controls whether encrypted rooms and events are allowed.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub allow_encryption: bool,

	/// Controls whether locally-created rooms should be end-to-end encrypted by
	/// default. This option is equivalent to the one found in Synapse.
	///
	/// Options:
	/// - "all": All created rooms are encrypted.
	/// - "invite": Any room created with `private_chat` or
	///   `trusted_private_chat` presets.
	/// - "none": Explicit value for no effect.
	/// - Other values default to no effect.
	///
	/// reloadable: yes
	/// default: "none"
	#[serde(default)]
	pub encryption_enabled_by_default_for_room_type: Option<String>,

	/// Set this to true to require authentication on the normally
	/// unauthenticated profile retrieval endpoints (GET)
	/// "/_matrix/client/v3/profile/{userId}".
	///
	/// This can prevent profile scraping.
	/// reloadable: yes
	#[serde(default)]
	pub require_auth_for_profile_requests: bool,

	/// Maximum fields retained in a remote user's cached profile.
	///
	/// Includes displayname and avatar_url and must be greater than zero.
	/// A refresh exceeding this count is rejected without changing the cache.
	/// Lowering it leaves existing profiles intact; their refreshes must fit
	/// the new limit.
	///
	/// reloadable: yes
	/// default: 100
	#[serde(default = "default_max_remote_profile_fields")]
	pub max_remote_profile_fields: usize,

	/// Maximum selected profile fields in a sync request or stored filter.
	///
	/// Counts raw selectors before duplicate removal for sliding and legacy sync
	/// and must be greater than zero.
	/// Requests exceeding this count or the 255-byte name limit are rejected.
	/// Sliding sync retains normalized selections across omitted selectors;
	/// after lowering the limit, an oversized selection requires replacement
	/// or a restarted connection.
	/// Unfiltered sliding sync profiles are not limited by this count.
	///
	/// reloadable: yes
	/// default: 64
	#[serde(default = "default_max_profile_fields_per_request")]
	pub max_profile_fields_per_request: usize,

	/// Allow standard users to set or clear their display names through the
	/// client profile API.
	///
	/// Server admins and appservices are always allowed to change display
	/// names.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub enable_set_displayname: bool,

	/// Restrict client profile retrieval to the user themselves or users who
	/// currently share a joined room.
	///
	/// Appservices are exempt, and other users are refused with 403
	/// `M_FORBIDDEN`. The server refuses to start, or to reload its
	/// configuration, with this enabled unless
	/// `require_auth_for_profile_requests` is enabled too. Federation profile
	/// lookups are unaffected; `allow_inbound_profile_lookup_federation_requests`
	/// controls those.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub limit_profile_requests_to_users_who_share_rooms: bool,

	/// Preserve per-room profile overrides during a global profile update.
	///
	/// When `true` (default), a profile change (displayname or avatar_url)
	/// arriving via the profile endpoints skips rooms whose current
	/// `m.room.member` already differs from the user's prior global
	/// profile. This is the natural behavior users expect after setting a
	/// per-room nickname or avatar with a client's `/myroomnick`-style
	/// command: a subsequent global change does not clobber the override.
	///
	/// Set to `false` to always rewrite every joined room's member event
	/// to match the new global profile. That matches the literal spec
	/// reading.
	///
	/// MSC4466 lets clients pick this per request via the
	/// `org.matrix.msc4466.propagate_to` query parameter
	/// (`all` / `unchanged` / `none`); an explicit value overrides this
	/// default in either direction.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub preserve_room_profile_overrides: bool,

	/// Set this to true to allow your server's public room directory to be
	/// federated. Set this to false to protect against /publicRooms spiders,
	/// but will forbid external users from viewing your server's public room
	/// directory. If federation is disabled entirely (`allow_federation`), this
	/// is inherently false.
	/// reloadable: yes
	#[serde(default)]
	pub allow_public_room_directory_over_federation: bool,

	/// Set this to true to allow your server's public room directory to be
	/// queried without client authentication (access token) through the Client
	/// APIs. Set this to false to protect against /publicRooms spiders.
	/// reloadable: yes
	#[serde(default)]
	pub allow_public_room_directory_without_auth: bool,

	/// Allows room directory searches to match on partial room_id's when the
	/// search term starts with '!'.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub allow_public_room_search_by_id: bool,

	/// Set this to false to limit results of rooms when searching by ID to
	/// those that would be found by an alias or other query; specifically
	/// those listed in the public rooms directory. By default this is set to
	/// true allowing any joinable room to match. This satisfies the Principle
	/// of Least Expectation when pasting a room_id into a search box with
	/// intent to join; many rooms simply opt-out of public listings. Therefor
	/// to prevent this feature from abuse, knowledge of several characters of
	/// the room_id is required before any results are returned.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub allow_unlisted_room_search_by_id: bool,

	/// Show all local users in user directory.
	///
	/// With this set to false, only users in public rooms or those that share
	/// a room with the user making the search will be shown. Appservice
	/// senders and users in exclusive appservice user namespaces stay hidden
	/// unless `show_appservice_users_in_user_directory` is also enabled.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub show_all_local_users_in_user_directory: bool,

	/// Include appservice senders and users in exclusive appservice user
	/// namespaces in user directory searches.
	///
	/// They remain subject to the normal room visibility rules unless
	/// `show_all_local_users_in_user_directory` is also enabled. Synapse has
	/// no equivalent and always hides them.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub show_appservice_users_in_user_directory: bool,

	/// Allow guest users to access TURN credentials.
	///
	/// This is the equivalent of Synapse's `turn_allow_guests` config option.
	/// Setting this to true allows guest users to call the endpoint
	/// `/_matrix/client/v3/voip/turnServer`.
	/// reloadable: yes
	#[serde(default)]
	pub turn_allow_guests: bool,

	/// Set this to true to lock down your server's public room directory and
	/// only allow admins to publish rooms to the room directory. Unpublishing
	/// is still allowed by all users with this enabled.
	/// reloadable: yes
	#[serde(default)]
	pub lockdown_public_room_directory: bool,

	/// Set this to true to allow federating device display names / allow
	/// external users to see your device display name. If federation is
	/// disabled entirely (`allow_federation`), this is inherently false. For
	/// privacy reasons, this is best left disabled.
	/// reloadable: yes
	#[serde(default)]
	pub allow_device_name_federation: bool,

	/// Config option to allow or disallow incoming federation requests that
	/// obtain the profiles of our local users from
	/// `/_matrix/federation/v1/query/profile`
	///
	/// Increases privacy of your local user's such as display names, but some
	/// remote users may get a false "this user does not exist" error when they
	/// try to invite you to a DM or room. Also can protect against profile
	/// spiders.
	///
	/// This is inherently false if `allow_federation` is disabled
	/// reloadable: yes
	#[serde(
		default = "true_fn",
		alias = "allow_profile_lookup_federation_requests"
	)]
	pub allow_inbound_profile_lookup_federation_requests: bool,

	/// Configures Matrix discovery documents and related endpoints.
	///
	/// Values are read from the separate `[global.well_known]` section. Client,
	/// server, support, and MatrixRTC responses consume these settings.
	// external structure; separate section
	#[serde(default)]
	pub well_known: WellKnownConfig,

	/// Enables OTLP span export for Jaeger-compatible tracing.
	///
	/// A build with performance measurements installs an OpenTelemetry layer
	/// when this is enabled. It defaults to false, and `jaeger_filter` selects
	/// the exported spans.
	#[serde(default)]
	pub allow_jaeger: bool,

	/// default: "info"
	#[serde(default = "default_jaeger_filter")]
	pub jaeger_filter: String,

	/// If the 'perf_measurements' compile-time feature is enabled, enables
	/// collecting folded stack trace profile of tracing spans using
	/// tracing_flame. The resulting profile can be visualized with inferno[1],
	/// speedscope[2], or a number of other tools.
	///
	/// [1]: https://github.com/jonhoo/inferno
	/// [2]: www.speedscope.app
	#[serde(default)]
	pub tracing_flame: bool,

	/// default: "info"
	#[serde(default = "default_tracing_flame_filter")]
	pub tracing_flame_filter: String,

	/// default: "./tracing.folded"
	#[serde(default = "default_tracing_flame_output_path")]
	pub tracing_flame_output_path: String,

	#[cfg(not(doctest))]
	/// Examples:
	///
	/// - No proxy (default):
	///
	///       proxy = "none"
	///
	/// - For global proxy, create the section at the bottom of this file:
	///
	///       [global.proxy]
	///       global = { url = "socks5h://localhost:9050" }
	///
	/// - To proxy some domains:
	///
	///       [global.proxy]
	///       [[global.proxy.by_domain]]
	///       url = "socks5h://localhost:9050"
	///       include = ["*.onion", "matrix.myspecial.onion"]
	///       exclude = ["*.myspecial.onion"]
	///
	/// Include vs. Exclude:
	///
	/// - If include is an empty list, it is assumed to be `["*"]`.
	///
	/// - If a domain matches both the exclude and include list, the proxy will
	///   only be used if it was included because of a more specific rule than
	///   it was excluded. In the above example, the proxy would be used for
	///   `ordinary.onion`, `matrix.myspecial.onion`, but not
	///   `hello.myspecial.onion`.
	///
	/// default: "none"
	#[serde(default)]
	pub proxy: ProxyConfig,

	/// Max log level for tuwunel. Allows debug, info, warn, or error.
	///
	/// See also:
	/// https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives
	///
	/// **Caveat**:
	/// For release builds, the tracing crate is configured to only implement
	/// levels higher than error to avoid unnecessary overhead in the compiled
	/// binary from trace macros. For debug builds, this restriction is not
	/// applied.
	///
	/// default: "info"
	#[serde(default = "default_log")]
	pub log: String,

	/// Output logs with ANSI colours.
	///
	/// Colours are suppressed while entries are submitted to journald, which
	/// takes the formatted line verbatim and reads control bytes in it as
	/// binary rather than text.
	#[serde(default = "true_fn", alias = "log_colours")]
	pub log_colors: bool,

	/// Sets the log format to compact mode.
	#[serde(default)]
	pub log_compact: bool,

	/// Configures the span events which will be outputted with the log.
	///
	/// default: "none"
	#[serde(default = "default_log_span_events")]
	pub log_span_events: String,

	/// Configures whether TUWUNEL_LOG EnvFilter matches values using regular
	/// expressions. See the tracing_subscriber documentation on Directives.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub log_filter_regex: bool,

	/// Toggles the display of ThreadId in tracing log output.
	///
	/// default: false
	#[serde(default)]
	pub log_thread_ids: bool,

	/// Redirects logging to standard error (stderr). The default is false for
	/// stdout. For those using our systemd features the redirection to stderr
	/// occurs as necessary and setting this option should not be required. We
	/// offer this option for all other users who desire such redirection.
	///
	/// default: false
	#[serde(default)]
	pub log_to_stderr: bool,

	/// Submits log output directly to the journald socket instead of the
	/// console when running under systemd. Each entry carries its actual
	/// severity as the journal priority, so tools such as `journalctl
	/// --priority warning` catch Tuwunel's warnings and errors; console output
	/// is captured by journald at a single fixed priority instead. The message
	/// is formatted exactly as the console formats it, span fields included,
	/// while the target, source location and every tracing field are attached
	/// as journal fields, the latter under an `F_` prefix for queries such as
	/// `journalctl F_ROOM_ID='!room:example.com'`. This option has no effect
	/// when not running under systemd, and the console is kept when the
	/// journald socket cannot be opened.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub log_journald: bool,

	/// Setting to false disables the logging/tracing system at a lower level.
	/// In contrast to configuring an empty `log` string where the system is
	/// still operating but muted, when this option is false the system was not
	/// initialized and is not operating. Changing this option has no effect
	/// after startup. This option is intended for developers and expert use
	/// only: configuring an empty log string is preferred over using this.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub log_enable: bool,

	/// Setting to false disables the logging/tracing system at a lower level
	/// similar to `log_enable`. In this case the system is configured normally,
	/// but not registered as the global handler in the final steps. This option
	/// is for developers and expert use only.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub log_global_default: bool,

	/// OpenID token expiration/TTL in seconds.
	///
	/// These are the OpenID tokens that are primarily used for Matrix account
	/// integrations (e.g. Vector Integrations in Element), *not* OIDC/OpenID
	/// Connect/etc.
	///
	/// reloadable: yes
	/// default: 3600
	#[serde(default = "default_openid_token_ttl")]
	pub openid_token_ttl: u64,

	/// Allow an existing session to mint a login token for another client.
	/// This requires interactive authentication, but has security ramifications
	/// as a malicious client could use the mechanism to spawn more than one
	/// session. Enabled by default.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub login_via_existing_session: bool,

	/// Whether to enable the login token route to accept login tokens at all.
	/// Login tokens may be generated by the server for authorization flows such
	/// as SSO; disabling tokens may break such features.
	///
	/// This option is distinct from `login_via_existing_session` and does not
	/// carry the same security implications; the intent is to leave this
	/// enabled while disabling the former to prevent clients from commanding
	/// login token creation but without preventing the server from doing so.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub login_via_token: bool,

	/// Whether to enable login using traditional user/password authorization
	/// flow.
	///
	/// Set this option to false if you intend to allow logging in only using
	/// other mechanisms, such as SSO.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub login_with_password: bool,

	/// Configures request rate limits, one `[global.rate_limiting]` subsection
	/// per kind of request.
	// external structure; separate section
	#[serde(default)]
	pub rate_limiting: RateLimits,

	/// Login token expiration/TTL in milliseconds.
	///
	/// These are short-lived tokens for the m.login.token endpoint.
	/// This is used to allow existing sessions to create new sessions.
	/// see login_via_existing_session.
	///
	/// reloadable: yes
	/// default: 120000
	#[serde(default = "default_login_token_ttl")]
	pub login_token_ttl: u64,

	/// Access token TTL in seconds.
	///
	/// For clients that support refresh-tokens, the access-token provided on
	/// login will be invalidated after this amount of time and the client will
	/// be soft-logged-out until refreshing it.
	///
	/// reloadable: yes
	/// default: 604800
	#[serde(default = "default_access_token_ttl")]
	pub access_token_ttl: u64,

	/// Refresh token TTL in seconds.
	///
	/// Refresh tokens are rejected once this lifetime elapses. Whether the
	/// deadline slides forward on each use or stays fixed at issuance is
	/// controlled by `refresh_token_idle_only`. The default of `0` disables
	/// refresh-token expiry entirely; a typical enabled value is `259200`
	/// (three days).
	///
	/// reloadable: yes
	/// default: 0
	#[serde(default)]
	pub refresh_token_ttl: u64,

	/// Whether `refresh_token_ttl` acts as an idle timeout or an absolute
	/// session lifetime.
	///
	/// When `true` (default), each successful refresh resets the deadline to
	/// `now + refresh_token_ttl`. A session in continuous use never expires.
	/// When `false`, the deadline is fixed at first issuance and rotation
	/// carries it forward, forcing re-auth after `refresh_token_ttl`
	/// regardless of activity. This setting applies only to tokens issued with
	/// a nonzero refresh-token TTL; a zero TTL issues tokens without expiry.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub refresh_token_idle_only: bool,

	/// Whether refresh-token expiry triggers a hard logout instead of a soft
	/// one.
	///
	/// When `false` (default), an expired refresh token is rejected with
	/// `M_UNKNOWN_TOKEN` carrying `soft_logout: true`. The client can preserve
	/// E2EE keys and local state, then re-authenticate to resume the same
	/// device.
	///
	/// When `true`, the device is removed entirely on expiry: the access
	/// token is invalidated, the device record is deleted, and the client is
	/// signalled with `soft_logout: false`. The next session is a brand-new
	/// device, so the client cannot recover E2EE history from local state
	/// alone; this is the CWE-613 stance and trades usability for that
	/// guarantee. Tokens issued with a zero refresh-token TTL do not expire,
	/// and changing the current TTL does not erase an expiry already stored.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub refresh_token_hard_logout: bool,

	/// Grace window in seconds for a benign refresh-token double-submit.
	///
	/// After a refresh token rotates, the spent token is retained for one
	/// generation so a later reuse is detectable. If that spent token is
	/// presented again within this window while its successor is still the
	/// device's current refresh token, the request is treated as a client that
	/// lost the rotated response: a fresh access token is issued for the
	/// unchanged refresh token rather than revoking the device. Outside the
	/// window, or once the chain has advanced, a replayed refresh token revokes
	/// the device as a suspected compromise. Set to `0` to treat every reuse as
	/// a compromise.
	///
	/// reloadable: yes
	/// default: 15
	#[serde(default = "default_refresh_token_reuse_grace")]
	pub refresh_token_reuse_grace: u64,

	/// Whether a detected refresh-token reuse revokes the device.
	///
	/// When true (default), presenting a refresh token that was already rotated
	/// (outside the `refresh_token_reuse_grace` window) removes the device, the
	/// RFC 6819 stance that treats reuse as a compromised session. When false,
	/// the replayed request is rejected but the device is left intact, the
	/// laxer behaviour an operator fronting another OAuth client may prefer.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub refresh_token_reuse_revoke: bool,

	/// Enable native registration and login on the built-in OIDC provider
	/// (next-gen auth), authenticating Matrix clients against this server's own
	/// accounts without a third-party `identity_provider`.
	///
	/// When false (default), the OIDC server runs only to broker for a
	/// configured `identity_provider`, redirecting users to that upstream IdP.
	/// When true, an authorization request that selects no provider is served a
	/// login or registration page; `well_known.client` must be set. The login
	/// page offers local accounts and each configured identity provider (one
	/// single sign-on entry under `single_sso` or `sso_custom_providers_page`),
	/// and an explicit `idp_id` goes directly to that provider. Registration
	/// here honors `allow_registration`, the registration token, and
	/// `registration_terms` exactly as the client registration endpoint does.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub oidc_native_auth: bool,

	/// Require OIDC clients (next-gen auth) to request an MSC2967 device scope.
	///
	/// When false, a client that omits the `urn:matrix:client:device:<id>`
	/// scope is assigned a server-generated device id, which is echoed back in
	/// the granted scope. When true, the authorization-code grant is rejected
	/// unless the client supplies a device scope, per the MSC2967 expectation
	/// that the client owns its device id.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub oidc_require_device_scope: bool,

	/// Require PKCE (RFC 7636) with the S256 method on the OIDC
	/// authorization-code grant.
	///
	/// When true, the authorize endpoint rejects a request that carries no
	/// `code_challenge`, as MSC2964 mandates for public clients. A present
	/// challenge must always use S256; the `plain` method is rejected
	/// regardless of this setting. Set to false only as a transition escape
	/// hatch for a legacy client that cannot send a challenge.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub oidc_require_pkce: bool,

	/// Reject an OIDC authorization-code grant that requests a scope this
	/// server does not recognise, instead of narrowing the granted scope down
	/// to the recognised tokens.
	///
	/// When false (default), an unrecognised scope token is dropped and the
	/// narrowed `scope` is echoed back to the client per RFC 6749. When true,
	/// an unrecognised scope is rejected. `openid` and the MSC2967 device and
	/// api scopes (both spellings) are always recognised.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub oidc_strict_scope: bool,

	/// Initial access token required to register an OIDC client dynamically
	/// (RFC 7591).
	///
	/// When set, the registration endpoint requires the caller to present this
	/// token as an `Authorization: Bearer` credential. No Matrix client sends
	/// one, so any value here blocks next-gen auth login for every ordinary
	/// client, Element X included. Leave it empty unless every OAuth client on
	/// this server is registered out of band.
	///
	/// display: sensitive
	/// reloadable: yes
	/// default:
	#[serde(default)]
	pub oidc_registration_access_token: String,

	/// Allowlist of hostnames permitted in a dynamically-registered OIDC
	/// client's redirect_uris.
	///
	/// When non-empty, every redirect_uri presented at registration must match
	/// this list or the registration is rejected. A redirect_uri with a host
	/// matches an entry naming that host, compared case-insensitively; a
	/// private-use scheme carries no host (RFC 8252, as in
	/// `io.element.android:/`) and matches an entry naming the scheme, so
	/// `["element.io", "io.element.android"]` covers a client on both web and
	/// mobile.
	///
	/// The default (empty) imposes no restriction at registration, but then
	/// vouches for no client either, so every sign-in is subject to
	/// `oidc_require_client_approval`. Listing an entry vouches for whatever
	/// answers to it, so a scheme entry covers any app on the device claiming
	/// that scheme (RFC 8252 §8.6) and a host entry covers every path on it.
	///
	/// reloadable: yes
	/// default: []
	#[serde(default)]
	pub oidc_registration_allowed_redirect_hosts: Vec<String>,

	/// Ask the user to approve an unvetted OIDC client before its
	/// authorization code is issued.
	///
	/// Dynamic client registration is open by default, so anyone can register a
	/// client with a redirect target they control and send an authorization
	/// link to one of your users; without this prompt a single click hands that
	/// client an authorization code for the account. When true (default), a
	/// sign-in whose redirect target is not vetted shows an approve/deny page
	/// naming the client first.
	///
	/// A client is vetted when its redirect host or private-use scheme appears
	/// in `oidc_registration_allowed_redirect_hosts`, so listing the clients
	/// you serve restores a prompt-free sign-in. Because the prompt is a step
	/// a person takes, a server that shows it wants a `login_token_ttl` long
	/// enough to read the page.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub oidc_require_client_approval: bool,

	/// Require a `client_uri` in dynamic client registration requests
	/// (RFC 7591 / MSC2966).
	///
	/// When false (default), `client_uri` is optional; a client that supplies
	/// one still has it validated (https, host, no userinfo) and the other URLs
	/// in the request must share its host or a subdomain. When true, a
	/// registration without an https `client_uri` is rejected with
	/// `invalid_client_metadata`, enforcing the MSC2966 common-base model on
	/// every client.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub oidc_registration_require_client_uri: bool,

	/// Token-bucket refill rate (requests per second) for the OIDC endpoints.
	///
	/// Applies a shared per-client-IP throttle across the authorize, token,
	/// dynamic-registration and device-grant endpoints. The default of `0`
	/// disables the throttle, preserving open
	/// access; raise it together with `oidc_rc_burst_count` to protect a server
	/// exposed to a hostile network. The key is the client IP, so a rate low
	/// enough to bite a brute-force attempt can also throttle many users behind
	/// one NAT; size the burst accordingly.
	///
	/// reloadable: yes
	/// default: 0
	#[serde(default)]
	pub oidc_rc_per_second: u32,

	/// Token-bucket depth (burst size) for the OIDC endpoint throttle.
	///
	/// The number of requests a single client IP may make in a burst before the
	/// `oidc_rc_per_second` refill rate governs. Ignored while
	/// `oidc_rc_per_second` is `0`.
	///
	/// reloadable: yes
	/// default: 0
	#[serde(default)]
	pub oidc_rc_burst_count: u32,

	/// Enable the rendezvous session APIs used to sign in with a QR code
	/// (MSC4108 and MSC4388).
	///
	/// The rendezvous session relays the handshake between two devices before
	/// the OAuth device authorization grant completes the sign-in. This
	/// requires the built-in OIDC server. When disabled, clients hide the
	/// feature and the endpoints return an unrecognized response.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub rendezvous_enabled: bool,

	/// Maximum size in bytes of a rendezvous session payload.
	///
	/// QR sign-in handshake messages are normally much smaller than the
	/// default.
	///
	/// reloadable: yes
	/// default: 4096
	#[serde(default = "default_rendezvous_session_max_bytes")]
	pub rendezvous_session_max_bytes: usize,

	/// Seconds a rendezvous session lives after its last write.
	///
	/// Each update restarts the window, but the device displaying the QR
	/// times the whole sign-in against the expiry advertised at creation.
	/// The default leaves time for an interactive account login on the
	/// approval page.
	///
	/// reloadable: yes
	/// default: 600
	#[serde(default = "default_rendezvous_session_ttl")]
	pub rendezvous_session_ttl: u64,

	/// Maximum number of concurrent rendezvous sessions.
	///
	/// Creating a session beyond this limit evicts the oldest session instead
	/// of failing. A value of zero retains one session so creation remains
	/// available.
	///
	/// reloadable: yes
	/// default: 100
	#[serde(default = "default_rendezvous_max_sessions")]
	pub rendezvous_max_sessions: usize,

	/// Require an access token for MSC4388 discovery and session creation.
	///
	/// When disabled, clients without an access token may discover and create
	/// MSC4388 sessions. The MSC4108 endpoint remains open in either mode.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub rendezvous_authenticated_only: bool,

	/// Per-client-IP request refill rate for the MSC4388 rendezvous endpoints.
	///
	/// A value of zero is treated as one request per second.
	///
	/// reloadable: yes
	/// default: 10
	#[serde(default = "default_rendezvous_rc_per_second")]
	pub rendezvous_rc_per_second: u32,

	/// Token-bucket depth for the MSC4388 rendezvous request throttle.
	///
	/// This is the number of requests one client IP may make in a burst before
	/// `rendezvous_rc_per_second` governs. A value of zero is treated as one.
	///
	/// reloadable: yes
	/// default: 20
	#[serde(default = "default_rendezvous_rc_burst_count")]
	pub rendezvous_rc_burst_count: u32,

	/// Static TURN username to provide the client if not using a shared secret
	/// ("turn_secret"), It is recommended to use a shared secret over static
	/// credentials.
	/// reloadable: yes
	#[serde(default)]
	pub turn_username: String,

	/// Static TURN password to provide the client if not using a shared secret
	/// ("turn_secret"). It is recommended to use a shared secret over static
	/// credentials.
	///
	/// display: sensitive
	/// reloadable: yes
	#[serde(default)]
	pub turn_password: String,

	#[expect(clippy::doc_link_with_quotes)]
	/// Vector list of TURN URIs/servers to use.
	///
	/// Replace "example.turn.uri" with your TURN domain, such as the coturn
	/// "realm" config option. If using TURN over TLS, replace the URI prefix
	/// "turn:" with "turns:".
	///
	/// reloadable: yes
	/// example: ["turn:example.turn.uri?transport=udp",
	/// "turn:example.turn.uri?transport=tcp"]
	///
	/// default: []
	#[serde(default)]
	pub turn_uris: Vec<String>,

	/// TURN secret to use for generating the HMAC-SHA1 hash apart of username
	/// and password generation.
	///
	/// This is more secure, but if needed you can use traditional static
	/// username/password credentials.
	///
	/// display: sensitive
	/// reloadable: yes
	#[serde(default)]
	pub turn_secret: Option<String>,

	/// TURN secret to use that's read from the file path specified.
	///
	/// This takes priority over "turn_secret", and falls back to it when the
	/// file cannot be opened. Surrounding whitespace is trimmed off, so a
	/// trailing newline does not become part of the secret. A file which is
	/// present but blank resolves to no secret rather than falling back.
	///
	/// reloadable: yes
	/// example: "/etc/tuwunel/.turn_secret"
	pub turn_secret_file: Option<PathBuf>,

	/// TURN TTL, in seconds.
	///
	/// reloadable: yes
	/// default: 86400
	#[serde(default = "default_turn_ttl")]
	pub turn_ttl: u64,

	#[expect(clippy::doc_link_with_quotes)]
	/// List/vector of room IDs or room aliases that tuwunel will make newly
	/// registered users join. The rooms specified must be rooms that you have
	/// joined at least once on the server, and must be public.
	///
	/// reloadable: yes
	/// example: ["#tuwunel:grin.hu",
	/// "!l2xV0sd51lraysuRcsWVECge4NULaH3g-ou95vgDgiM"]
	///
	/// default: []
	#[serde(default = "Vec::new")]
	pub auto_join_rooms: Vec<OwnedRoomOrAliasId>,

	/// This is a password that can be configured that will let you login to the
	/// server bot account (currently `@conduit`) for emergency troubleshooting
	/// purposes such as recovering/recreating your admin room, or inviting
	/// yourself back.
	///
	/// See https://tuwunel.chat/troubleshooting.html#lost-access-to-admin-room
	/// for other ways to get back into your admin room.
	///
	/// Once this password is unset, all sessions will be logged out for
	/// security purposes.
	///
	/// example: "F670$2CP@Hw8mG7RY1$%!#Ic7YA"
	///
	/// display: sensitive
	pub emergency_password: Option<String>,

	/// Suffix stripped from the gateway URL before constructing a push request.
	///
	/// reloadable: yes
	/// default: "/_matrix/push/v1/notify"
	#[serde(default = "default_notification_push_path")]
	pub notification_push_path: String,

	/// For compatibility and special purpose use only. Setting this option to
	/// true will not filter messages sent to pushers based on rules or actions.
	/// Everything will be sent to the pusher. This option is offered for
	/// several reasons, but should not be necessary:
	/// - Bypass to workaround bugs or outdated server-side ruleset support.
	/// - Allow clients to evaluate pushrules themselves (due to the above).
	/// - Hosting or companies which have custom pushers and internal needs.
	///
	/// Note that setting this option to true will not affect the record of
	/// notifications found in the notifications pane.
	/// reloadable: yes
	#[serde(default)]
	pub push_everything: bool,

	/// Evaluate the `im.nheko.msc3664.related_event_match` push rule condition,
	/// which matches on a property of the event that an incoming event relates
	/// to.
	///
	/// A user can then write a push rule that notifies for replies or reactions
	/// to their own messages, which no other condition can express. Enabling
	/// this costs one extra event lookup for every event carrying a relation.
	///
	/// The default `.im.nheko.msc3664.reply` push rule uses the condition.
	/// Disabling evaluation leaves the rule present but unable to match
	/// replies, while clients implementing MSC3664 may still evaluate it
	/// locally.
	///
	/// reloadable: yes
	#[serde(default)]
	pub msc3664_related_event_match: bool,

	/// Setting to false disables the heroes calculation made by sliding and
	/// legacy client sync. The heroes calculation is mandated by the Matrix
	/// specification and your client may not operate properly unless this
	/// option is set to true.
	///
	/// This option is intended for custom software deployments seeking purely
	/// to minimize unused resources; the overall savings are otherwise
	/// negligible.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub calculate_heroes: bool,

	/// Allow local (your server only) presence updates/requests.
	///
	/// Note that presence on tuwunel is very fast unlike Synapse's. If using
	/// outgoing presence, this MUST be enabled.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub allow_local_presence: bool,

	/// Allow incoming federated presence updates/requests.
	///
	/// This option receives presence updates from other servers, but does not
	/// send any unless `allow_outgoing_presence` is true. Note that presence on
	/// tuwunel is very fast unlike Synapse's.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub allow_incoming_presence: bool,

	/// Allow outgoing presence updates/requests.
	///
	/// This option sends presence updates to other servers, but does not
	/// receive any unless `allow_incoming_presence` is true. Note that presence
	/// on tuwunel is very fast unlike Synapse's. If using outgoing presence,
	/// you MUST enable `allow_local_presence` as well.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub allow_outgoing_presence: bool,

	/// How many seconds without presence updates before you become idle.
	/// Defaults to 5 minutes.
	///
	/// default: 300
	#[serde(default = "default_presence_idle_timeout_s")]
	pub presence_idle_timeout_s: u64,

	/// How many seconds without presence updates before you become offline.
	/// Defaults to 30 minutes.
	///
	/// default: 1800
	#[serde(default = "default_presence_offline_timeout_s")]
	pub presence_offline_timeout_s: u64,

	/// Enable the presence idle timer for remote users.
	///
	/// Disabling is offered as an optimization for servers participating in
	/// many large rooms or when resources are limited. Disabling it may cause
	/// incorrect presence states (i.e. stuck online) to be seen for some remote
	/// users.
	#[serde(default = "true_fn")]
	pub presence_timeout_remote_users: bool,

	/// Suppresses push notifications for users marked as active. (Experimental)
	///
	/// When enabled, users with `Online` presence and recent activity
	/// (based on presence state and sync activity) won’t receive push
	/// notifications, reducing duplicate alerts while they're active
	/// on another client.
	///
	/// Disabled by default to preserve legacy behavior.
	/// reloadable: yes
	#[serde(default)]
	pub suppress_push_when_active: bool,

	/// Allow receiving incoming read receipts from remote servers.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub allow_incoming_read_receipts: bool,

	/// Allow sending read receipts to remote servers.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub allow_outgoing_read_receipts: bool,

	/// Allow outgoing typing updates to federation.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub allow_outgoing_typing: bool,

	/// Allow incoming typing updates from federation.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub allow_incoming_typing: bool,

	/// Maximum time federation user can indicate typing.
	///
	/// reloadable: yes
	/// default: 30
	#[serde(default = "default_typing_federation_timeout_s")]
	pub typing_federation_timeout_s: u64,

	/// Minimum time local client can indicate typing. This does not override a
	/// client's request to stop typing. It only enforces a minimum value in
	/// case of no stop request.
	///
	/// reloadable: yes
	/// default: 15
	#[serde(default = "default_typing_client_timeout_min_s")]
	pub typing_client_timeout_min_s: u64,

	/// Maximum time local client can indicate typing.
	///
	/// reloadable: yes
	/// default: 45
	#[serde(default = "default_typing_client_timeout_max_s")]
	pub typing_client_timeout_max_s: u64,

	/// Set to true to allow user type "guest" registrations. Some clients like
	/// Element attempt to register guest users automatically.
	/// reloadable: yes
	#[serde(default)]
	pub allow_guest_registration: bool,

	/// Set to true to log guest registrations in the admin room. Note that
	/// these may be noisy or unnecessary if you're a public homeserver.
	/// reloadable: yes
	#[serde(default)]
	pub log_guest_registrations: bool,

	/// Set to true to allow guest registrations/users to auto join any rooms
	/// specified in `auto_join_rooms`.
	/// reloadable: yes
	#[serde(default)]
	pub allow_guests_auto_join_rooms: bool,

	/// List of forbidden server names via regex patterns that we will block all
	/// outgoing federated room directory requests for. Useful for preventing
	/// our users from wandering into bad servers or spaces.
	///
	/// reloadable: yes
	/// example: ["badserver\.tld$", "badphrase", "19dollarfortnitecards"]
	///
	/// default: []
	#[serde(default, with = "serde_regex")]
	pub forbidden_remote_room_directory_server_names: RegexSet,

	/// List of forbidden username patterns/strings.
	///
	/// Regex can be used or explicit contains matches can be done by just
	/// specifying the words (see example).
	///
	/// This is checked upon username availability check, registration, and
	/// startup as warnings if any local users in your database have a forbidden
	/// username.
	///
	/// reloadable: yes
	/// example: ["administrator", "b[a4]dusernam[3e]", "badphrase"]
	///
	/// default: []
	#[serde(default, with = "serde_regex")]
	pub forbidden_usernames: RegexSet,

	/// Allow admins to run commands outside the admin room ("#admins") by
	/// prefixing a message with "\!admin" or "\\!admin" and a normal command.
	///
	/// The reply is publicly visible to the room, originating from the sender.
	/// Disabled by default: the server cannot tell a command an admin typed
	/// from one in a message the admin forwarded or a bot on their account
	/// reposted. Message formatting can also hide the command from the admin.
	///
	/// reloadable: yes
	/// example: \\!admin debug ping puppygock.gay
	#[serde(default)]
	pub admin_escape_commands: bool,

	/// Automatically activate the tuwunel admin room console / CLI on
	/// startup. This option can also be enabled with `--console` tuwunel
	/// argument. Activation requires standard input to be a terminal.
	#[serde(default)]
	pub admin_console_automatic: bool,

	#[expect(clippy::doc_link_with_quotes)]
	/// List of admin commands to execute on startup.
	///
	/// This option can also be configured with the `--execute` tuwunel
	/// argument and can take standard shell commands and environment variables
	///
	/// For example: `./tuwunel --execute "server admin-notice tuwunel has
	/// started up at $(date)"`
	///
	/// example: admin_execute = ["debug ping puppygock.gay", "debug echo hi"]`
	///
	/// default: []
	#[serde(default)]
	pub admin_execute: Vec<String>,

	/// Ignore errors in startup commands.
	///
	/// If false, tuwunel will error and fail to start if an admin execute
	/// command (`--execute` / `admin_execute`) fails.
	/// reloadable: yes
	#[serde(default)]
	pub admin_execute_errors_ignore: bool,

	/// List of admin commands to execute on SIGUSR2.
	///
	/// Similar to admin_execute, but these commands are executed when the
	/// server receives SIGUSR2 on supporting platforms.
	///
	/// reloadable: yes
	/// default: []
	#[serde(default)]
	pub admin_signal_execute: Vec<String>,

	/// Controls the max log level for admin command log captures (logs
	/// generated from running admin commands). Defaults to "info" on release
	/// builds, else "debug" on debug builds.
	///
	/// reloadable: yes
	/// default: "info"
	#[serde(default = "default_admin_log_capture")]
	pub admin_log_capture: String,

	/// The default room tag to apply on the admin room.
	///
	/// On some clients like Element, the room tag "m.server_notice" is a
	/// special pinned room at the very bottom of your room list. The tuwunel
	/// admin room can be pinned here so you always have an easy-to-access
	/// shortcut dedicated to your admin room.
	///
	/// reloadable: yes
	/// default: "m.server_notice"
	#[serde(default = "default_admin_room_tag")]
	pub admin_room_tag: String,

	/// The localpart of the server's administrative user.
	///
	/// This identity is durable after first boot. Changing it on an existing
	/// database is unsupported and prevents startup.
	///
	/// default: "conduit"
	#[serde(default = "default_server_user_localpart")]
	pub server_user_localpart: ServerUserLocalpart,

	/// The room that user, room, and event reports are posted to, instead of
	/// the admin room. Accepts a room ID or room alias; the server user must be
	/// joined with permission to post there. Reports fall back to the admin
	/// room when this is unset, cannot be resolved, or the server user is not a
	/// member.
	///
	/// reloadable: yes
	/// default: (none)
	#[serde(default)]
	pub report_room: Option<OwnedRoomOrAliasId>,

	/// Whether to grant the first user to register admin privileges by joining
	/// them to the admin room. Note that technically the next user to register
	/// when the admin room is empty (or only contains the server-user) is
	/// granted, and only when the admin room is enabled.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub grant_admin_to_first_user: bool,

	/// Whether the admin room is created on first startup. Users should not set
	/// this to false. Developers can set this to false during integration tests
	/// to reduce activity and output.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub create_admin_room: bool,

	/// Whether to enable federation on the admin room. This cannot be changed
	/// after the admin room is created.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub federate_admin_room: bool,

	/// Sentry.io crash/panic reporting, performance monitoring/metrics, etc.
	/// This is NOT enabled by default. tuwunel's default Sentry reporting
	/// endpoint domain is `o4509498990067712.ingest.us.sentry.io`.
	#[serde(default)]
	pub sentry: bool,

	/// Sentry reporting URL, if a custom one is desired.
	///
	/// display: sensitive
	/// default: ""
	#[serde(default = "default_sentry_endpoint")]
	pub sentry_endpoint: Option<Url>,

	/// Report your tuwunel server_name in Sentry.io crash reports and
	/// metrics.
	#[serde(default)]
	pub sentry_send_server_name: bool,

	/// Performance monitoring/tracing sample rate for Sentry.io.
	///
	/// Sentry reads this as a fraction, so it must lie between 0.0 and 1.0
	/// inclusive when Sentry is enabled. High values may impact performance;
	/// 0.0 disables sampling.
	///
	/// default: 0.15
	#[serde(default = "default_sentry_traces_sample_rate")]
	pub sentry_traces_sample_rate: f32,

	/// Whether to attach a stacktrace to Sentry reports.
	#[serde(default)]
	pub sentry_attach_stacktrace: bool,

	/// Send panics to Sentry. This is true by default, but Sentry has to be
	/// enabled. The global `sentry` config option must be enabled to send any
	/// data.
	#[serde(default = "true_fn")]
	pub sentry_send_panic: bool,

	/// Send errors to sentry. This is true by default, but sentry has to be
	/// enabled. This option is only effective in release-mode; forced to false
	/// in debug-mode.
	#[serde(default = "true_fn")]
	pub sentry_send_error: bool,

	/// Controls the tracing log level for Sentry to send things like
	/// breadcrumbs and transactions
	///
	/// default: "info"
	#[serde(default = "default_sentry_filter")]
	pub sentry_filter: String,

	/// Enable the tokio-console. This option is only relevant to developers.
	///
	///	For more information, see:
	/// https://tuwunel.chat/development.html#debugging-with-tokio-console
	#[serde(default)]
	pub tokio_console: bool,

	/// Arbitrary argument vector for integration testing. Functionality in the
	/// server is altered or informed for the requirements of integration tests.
	/// - "smoke" performs a shutdown after startup admin commands rather than
	///   hanging on client handling.
	///
	/// display: hidden
	#[serde(default)]
	pub test: BTreeSet<String>,

	/// Indicates the server has started in maintenance mode. Historically
	/// maintenance mode has been enabled by the command line argument
	/// `--maintenance` which then sets various configuration items such as
	/// `listening=false` among others. That is still the case. This option was
	/// only added as a single source of truth that `--maintenance` mode is
	/// active.
	///
	/// This option must never be set manually.
	///
	/// display: hidden
	#[serde(default)]
	pub maintenance: bool,

	/// Controls whether admin room notices like account registrations, password
	/// changes, account deactivations, room directory publications, etc will be
	/// sent to the admin room. Update notices and normal admin command
	/// responses will still be sent.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub admin_room_notices: bool,

	/// Maximum number of message events an admin command's output may be split
	/// across as replies in the admin room. Output needing more events than
	/// this is uploaded to the media repository instead and returned as a text
	/// file attachment replying to the command. When 1, output which fits in a
	/// single event is posted as a single reply and anything larger becomes an
	/// attachment. When 0, output is always posted as an attachment regardless
	/// of size.
	///
	/// reloadable: yes
	/// default: 1
	#[serde(default = "default_admin_output_max_events")]
	pub admin_output_max_events: usize,

	/// Post admin command output into a thread on the command event rather than
	/// as replies. Output split across multiple events per
	/// `admin_output_max_events` is contained in a single thread; attachment
	/// outputs are posted into the thread as well.
	///
	/// reloadable: yes
	#[serde(default)]
	pub admin_output_threads: bool,

	/// Enables listener sockets; can be set to false to disable listening. This
	/// option is intended for developer/diagnostic purposes only.
	#[serde(default = "true_fn")]
	pub listening: bool,

	/// Enables configuration reload when the server receives SIGUSR1 on
	/// supporting platforms.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub config_reload_signal: bool,

	/// Backport state-reset security fixes to all room versions.
	///
	/// This option applies the State Resolution 2.1 mitigation developed during
	/// project Hydra for room version 12 to all prior State Resolution 2.0 room
	/// versions (all room versions supported by this server). These mitigations
	/// increase resilience to state-resets without any new definition of
	/// correctness; therefor it is safe to set this to true for existing rooms.
	///
	/// Furthermore, state-reset attacks are not consistent as they result in
	/// rooms without any single consensus, therefor it is unnecessary to set
	/// this to false to match other servers which set this to false or simply
	/// lack support; even if replicating the post-reset state suffered by other
	/// servers is somehow desired.
	///
	/// This option exists for developer and debug use, and as a failsafe in
	/// lieu of hardcoding it.
	/// reloadable: yes
	#[serde(default = "true_fn")]
	pub hydra_backports: bool,

	/// Limits the number of One Time Keys per device (not per-algorithm). The
	/// reference implementation maintains 50 OTK's at any given time, therefor
	/// our default is at least five times that. There is no known reason for an
	/// administrator to adjust this value; it is provided here rather than
	/// hardcoding it.
	///
	/// reloadable: yes
	/// default: 256
	#[serde(default = "default_one_time_key_limit")]
	pub one_time_key_limit: usize,

	/// (EXPERIMENTAL) Setting this option to true replaces the list of identity
	/// providers displayed on a client's login page with a single button "Sign
	/// in with single sign-on" linking to the URL
	/// `/_matrix/client/v3/login/sso/redirect`. All configured providers are
	/// attempted for authorization. All authorizations associate with the same
	/// Matrix user. NOTE: All authorizations must succeed, as there is no
	/// reliable way to skip a provider.
	///
	/// This option is disabled by default, allowing the client to list
	/// configured providers and permitting privacy-conscious users to authorize
	/// only their choice.
	///
	/// Note that fluffychat always displays a single button anyway. You do not
	/// need to enable this to use fluffychat; instead we offer a
	/// default-provider option, see `default` in the provider config section.
	/// reloadable: yes
	#[serde(default)]
	pub single_sso: bool,

	/// Setting this option to true replaces the list of identity providers on
	/// the client's login screen with a single button "Sign in with single
	/// sign-on" linking to the URL `/_matrix/client/v3/login/sso/redirect`. The
	/// deployment is expected to intercept this URL with their reverse-proxy to
	/// provide a custom webpage listing providers; each entry linking or
	/// redirecting back to one of the configured identity providers at
	/// /_matrix/client/v3/login/sso/redirect/<client_id>`.
	///
	/// This option defaults to false, allowing the client to generate the list
	/// of providers or hide all SSO-related options when none configured.
	/// reloadable: yes
	#[serde(default)]
	pub sso_custom_providers_page: bool,

	/// From MSC3824:
	/// > If the client finds oauth_aware_preferred to be true then, assuming it
	/// > supports that auth type, it should present this as the only
	/// > login/registration method available to the user.
	/// reloadable: yes
	#[serde(default, alias = "sso_aware_preferred")]
	pub oidc_aware_preferred: bool,

	/// Directory containing appservice yaml registration files.
	///
	/// default: ""
	#[serde(default)]
	pub appservice_dir: Option<PathBuf>,

	/// Apply pending database migrations on startup. This option is intended for
	/// developer debugging and testing only. Never set this option to false
	/// unless you have been instructed to do so. Setting this option to false
	/// may cause permanent damage and permanent loss of data.
	///
	/// Any new database migrations will not be applied on startup, and the
	/// database schema version will not be adjusted. These migrations and
	/// schema changes may be expected by the current codebase but may not be
	/// available when this option is set to false.
	///
	/// Setting this option to false is accepted only after the required local
	/// state memo invalidation has run once. Start once with this option true if
	/// startup reports that prerequisite missing. With the marker present,
	/// false continues to skip migrations as configured.
	#[serde(default = "true_fn")]
	pub database_migrations: bool,

	/// Open a database whose schema version is newer than this build supports.
	///
	/// A database reporting a higher schema version than this build is normally
	/// refused, since opening it stamps the schema down to this build's version
	/// and may permanently lose data written by the newer build. Setting this
	/// to true overrides that refusal: the database opens, one-time migrations
	/// run, and the schema is stamped down to this build's version.
	///
	/// It has no effect when the discovered version is at or below this build's
	/// version, where migrations apply normally either way. It is also not
	/// needed to import a Conduit database or a fork of conduwuit; those are
	/// recognized by lineage and open without it.
	///
	/// This option is extremely dangerous and intended for developer debugging
	/// and testing only. Never set it unless you have been instructed to do so;
	/// it may cause permanent damage and permanent loss of data.
	#[serde(default)]
	pub force_migration: bool,

	/// Set this to true for excluding unencrypted rooms from the common-rooms
	/// calculation deciding the receivers of device list updates.
	///
	/// Setting this to true can help performance on very large homeservers,
	/// but it may not be spec compliant and risky for client expectations.
	/// reloadable: yes
	#[serde(default)]
	pub device_key_update_encrypted_rooms_only: bool,

	/// Defines named media storage providers.
	///
	/// Each map key names a provider, and each value selects a local or
	/// S3-compatible backend or disables the entry. Provider-specific settings
	/// live in separate sections.
	// external structure; separate section
	#[serde(default)]
	pub storage_provider: BTreeMap<String, StorageProvider>,

	/// Defines policy documents users must accept during registration.
	///
	/// Each map key is the policy identifier exposed in the `m.login.terms` UIA
	/// stage. An empty map leaves the terms stage disabled.
	// external structure; separate section
	#[serde(default)]
	pub registration_terms: BTreeMap<String, TermsPolicy>,

	/// Configures LDAP login integration.
	///
	/// Connection, bind, search, and attribute settings live in the separate
	/// `[global.ldap]` section. LDAP authentication is disabled by default.
	// external structure; separate section
	#[serde(default)]
	pub ldap: LdapConfig,

	/// Configures JSON Web Token login integration.
	///
	/// Key format, algorithm, claim validation, and user provisioning settings
	/// live in `[global.jwt]`. Token login is disabled by default.
	// external structure; separate section
	#[serde(default)]
	pub jwt: JwtConfig,

	/// Configures outbound SMTP email delivery.
	///
	/// Providing a connection URI enables the email subsystem. Registration
	/// flags determine when a verified address is required.
	// external structure; separate section
	#[serde(default)]
	pub smtp: SmtpConfig,

	/// Defines inline application service registrations.
	///
	/// Each map key names one registration and supplies its default identifier.
	/// The contained settings are converted to Matrix application service data.
	// external structure; separate section
	#[serde(default)]
	pub appservice: BTreeMap<String, AppService>,

	/// Defines OpenID Connect identity provider registrations.
	///
	/// Each entry configures client credentials, discovery, and account
	/// mapping. Its stable `client_id` identifies the provider while `brand`
	/// selects provider-specific defaults and workarounds.
	// external structure; separate sections
	#[serde(default, with = "identity_provider_serde")]
	pub identity_provider: BTreeMap<String, IdentityProvider>,

	#[serde(flatten)]
	#[expect(clippy::zero_sized_map_values)]
	// this is a catchall, the map shouldn't be zero at runtime
	catchall: BTreeMap<String, IgnoredAny>,

	/// Settings for cache.
	///
	/// display: section
	#[serde(default)]
	pub cache: Cache,

	/// Settings for database.
	///
	/// display: section
	#[serde(default)]
	pub database: Database,

	/// Settings for runtime.
	///
	/// display: section
	#[serde(default)]
	pub runtime: Runtime,

	/// Settings for http.
	///
	/// display: section
	#[serde(default)]
	pub http: Http,

	/// Settings for network.
	///
	/// display: section
	#[serde(default)]
	pub network: Network,

	/// Settings for media.
	///
	/// display: section
	#[serde(default)]
	pub media: Media,

	/// Settings for federation.
	///
	/// display: section
	#[serde(default)]
	pub federation: Federation,

	/// Settings for rooms.
	///
	/// display: section
	#[serde(default)]
	pub rooms: Rooms,

	/// Settings for sending.
	///
	/// display: section
	#[serde(default)]
	pub sending: Sending,
}

/// Configures direct TLS listener behavior.
///
/// Certificate and key paths must be supplied together. Optional dual-protocol
/// mode accepts encrypted and plain connections on the same listeners.
#[derive(Clone, Debug, Deserialize, Default)]
#[config_example_generator(filename = "tuwunel-example.toml", section = "global.tls")]
pub struct TlsConfig {
	/// Path to a valid TLS certificate file; this section requires a build with
	/// the `direct_tls` Cargo feature.
	///
	/// example: "/path/to/my/certificate.crt"
	pub certs: Option<String>,

	/// Path to a valid TLS certificate private key.
	///
	/// example: "/path/to/my/certificate.key"
	pub key: Option<String>,

	/// Controls whether listeners accept both HTTP and HTTPS.
	///
	/// Plain requests are served without redirecting them to HTTPS. This
	/// weakens transport security and is disabled by default.
	#[serde(default)]
	pub dual_protocol: bool,
}

/// Configures Matrix discovery documents and related response data.
///
/// Client and server fields drive the standard well-known responses. Support
/// contacts, policies, and MatrixRTC transports populate their corresponding
/// discovery data.
#[expect(rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize, Default)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.well_known",
	ignore = "support_contact support_policy",
	hidden = "support_role support_email support_mxid support_page support_pgp_key"
)]
pub struct WellKnownConfig {
	/// The server URL that the client well-known file will serve.
	///
	/// This should not contain a port, and should just be a valid HTTPS URL.
	/// While this is unset, `/.well-known/matrix/client` answers 404 and
	/// auto-discovery from the server name yields nothing, so the base URL has
	/// to reach clients some other way. Leave it unset only when a reverse
	/// proxy or another host publishes that file for this domain.
	///
	/// example: "https://matrix.example.com"
	pub client: Option<Url>,

	/// The server base domain of the URL with a specific port that the server
	/// well-known file will serve. This should contain a port at the end, and
	/// should not be a URL.
	///
	/// reloadable: yes
	/// example: "matrix.example.com:443"
	pub server: Option<OwnedServerName>,

	/// Defines contacts published by the support discovery endpoint.
	///
	/// Each map value becomes one contact while its key is only a config
	/// identifier. Legacy scalar support fields are appended separately.
	// external structure; separate section
	#[serde(default)]
	pub support_contact: BTreeMap<String, SupportContact>,

	/// Defines policies published by the support discovery endpoint.
	///
	/// Each map key becomes a policy identifier. The value supplies its version
	/// and localized documents.
	// external structure; separate section
	#[serde(default)]
	pub support_policy: BTreeMap<String, SupportPolicy>,

	/// The URL of the support web page. This and the below generate the content
	/// of `/.well-known/matrix/support`.
	///
	/// example: "https://example.com/support"
	pub support_page: Option<Url>,

	/// The name of the support role.
	///
	///
	/// display: hidden
	// This config option is hidden because [global.well_known.support_contact.<ID>] should be
	// used instead. However for compatibility purposes the config option will still function and
	// be prioritised first.
	pub support_role: Option<ContactRole>,

	/// The email address for the above support role.
	///
	///
	/// display: hidden
	// This config option is hidden because [global.well_known.support_contact.<ID>] should be
	// used instead. However for compatibility purposes the config option will still function and
	// be prioritised first.
	pub support_email: Option<String>,

	/// The Matrix User ID for the above support role.
	///
	/// display: hidden
	// This config option is hidden because [global.well_known.support_contact.<ID>] should be
	// used instead. However for compatibility purposes the config option will still function and
	// be prioritised first.
	pub support_mxid: Option<OwnedUserId>,

	/// The PGP key (i.e. OpenPGP) that one may use for encrypted communications
	/// for the above support role. The value must be a URI. Use a web URL
	/// pointing to the key (for example "https://example.com/key.asc"), an
	/// OPENPGPKEY DNS record ("dns:..."), or a fingerprint carried with the
	/// "openpgp4fpr:" scheme. A bare fingerprint without a scheme, or raw
	/// inlined key material, is rejected at startup.
	///
	/// As this is a spec proposal (MSC4439), the identifier/prefix for this
	/// field is currently "dev.zirco.msc4439.pgp_key"
	///
	/// display: hidden
	// This config option is hidden because [global.well_known.support_contact.<ID>] should be
	// used instead. However for compatibility purposes the config option will still function and
	// be prioritised first.
	pub support_pgp_key: Option<String>,

	/// LiveKit JWT endpoint.
	/// Required for Element Call / MatrixRTC (MSC4143).
	///
	/// Note: You must also set `client` above to your homeserver URL.
	///
	/// reloadable: yes
	/// default: ""
	#[serde(default)]
	pub livekit_url: Option<String>,

	/// Custom MatrixRTC transports.
	///
	/// If you're looking to setup Element Call / MatrixRTC with Livekit,
	/// you should not use this option and instead set `livekit_url`.
	/// This is only required if you want to configure a non-livekit MatrixRTC
	/// transport. There are no known client implementations that support any
	/// other transport types.
	///
	/// This option was previously the only way to configure a Livekit
	/// transport. It has been superseded by `livekit_url`.
	///
	/// Example:
	/// ```toml
	/// [global.well_known]
	/// client = "https://matrix.yourdomain.com"
	///
	/// [[global.well_known.rtc_transports]]
	/// type = "livekit"
	/// livekit_service_url = "https://livekit.yourdomain.com"
	/// ```
	///
	/// reloadable: yes
	/// default: []
	#[serde(default)]
	pub rtc_transports: Vec<serde_json::Value>,
}

/// Defines one policy published by the support discovery endpoint.
///
/// The enclosing map key supplies the policy identifier. Its version and
/// localized translations are emitted in the discovery response.
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.well_known.support_policy.<ID>",
	ignore = "policy_translation"
)]
pub struct SupportPolicy {
	/// Version string of the policy document.
	///
	/// example: "v6.7"
	/// reloadable: yes
	pub version: String,

	/// Maps language identifiers to localized policy documents.
	///
	/// Each value supplies the display name and URL for its language. The map
	/// is converted to the response's localized policy entries.
	// external structure; separate section
	pub policy_translation: BTreeMap<String, SupportPolicyTranslation>,
}

/// Defines one localized support policy document.
///
/// `name` is the user-facing title for this language. `url` points clients to
/// the corresponding policy text.
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.well_known.support_policy.<ID>.policy_translation.<LANG>"
)]
pub struct SupportPolicyTranslation {
	/// User friendly name of the policy document.
	///
	/// example: "Privacy Policy"
	/// reloadable: yes
	pub name: String,

	/// Link to the test of the policy document. A valid URL must be specified.
	///
	/// example: "https://website.local/privacy-policy"
	/// reloadable: yes
	pub url: Url,
}

/// Defines a policy document required during registration.
///
/// The enclosing map key becomes the policy identifier presented to clients.
/// Its version and translations form the `m.login.terms` stage parameters.
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.registration_terms.<ID>",
	ignore = "translations"
)]
pub struct TermsPolicy {
	/// Version of this policy document, presented to the client. Configuring
	/// any `[global.registration_terms.<ID>]` block makes registration
	/// require an `m.login.terms` stage listing every such document; the
	/// `<ID>` is the policy id sent to clients.
	///
	/// example: "1.2"
	/// reloadable: yes
	pub version: String,

	/// Maps language identifiers to localized registration policy documents.
	///
	/// Each value supplies the display name and HTTP or HTTPS URL for its
	/// language. These translations are presented in the terms stage.
	// external structure; separate section
	pub translations: BTreeMap<String, TermsPolicyTranslation>,
}

/// Defines one localized registration policy document.
///
/// `name` is the user-facing title for this language. `url` points clients to
/// the policy text whose acceptance is recorded.
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.registration_terms.<ID>.translations.<LANG>"
)]
pub struct TermsPolicyTranslation {
	/// User friendly name of the policy document in this language.
	///
	/// example: "Terms of Service"
	/// reloadable: yes
	pub name: String,

	/// Link to the text of the policy document. Must be a valid http(s) URL.
	///
	/// example: "https://example.org/terms-1.2-en.html"
	/// reloadable: yes
	pub url: Url,
}

impl From<SupportPolicyTranslation>
	for ruma::api::identity_service::tos::get_terms_of_service::v2::LocalizedPolicy
{
	fn from(conf: SupportPolicyTranslation) -> Self {
		Self {
			name: conf.name,
			url: conf.url.to_string(),
		}
	}
}

/// Defines a contact published by the support discovery endpoint.
///
/// Every contact has a Matrix support role. Email, Matrix ID, and OpenPGP key
/// fields provide optional communication channels.
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.well_known.support_contact.<ID>"
)]
pub struct SupportContact {
	/// The name of the support role.
	///
	/// example: "m.role.admin"
	pub role: ContactRole,

	/// The email address for the above support role.
	///
	/// example: "admin@example.com"
	pub email_address: Option<String>,

	/// The Matrix User ID for the above support role.
	///
	/// example "@admin:example.com"
	pub matrix_id: Option<OwnedUserId>,

	/// The PGP key (i.e. OpenPGP) that one may use for encrypted communications
	/// for the above support role. The value must be a URI. Use a web URL
	/// pointing to the key (for example "https://example.com/key.asc"), an
	/// OPENPGPKEY DNS record ("dns:..."), or a fingerprint carried with the
	/// "openpgp4fpr:" scheme. A bare fingerprint without a scheme, or raw
	/// inlined key material, is rejected at startup.
	///
	/// As this is a spec proposal (MSC4439), the identifier/prefix for this
	/// field is currently "dev.zirco.msc4439.pgp_key"
	///
	/// example: "openpgp4fpr:8B77919975EAFA5E2456EE03665FE73077489DB0"
	pub pgp_key: Option<String>,
}

impl From<SupportContact> for ruma::api::client::discovery::discover_support::Contact {
	fn from(conf: SupportContact) -> Self {
		Self {
			role: conf.role,
			matrix_id: conf.matrix_id,
			email_address: conf.email_address,
			pgp_key: conf.pgp_key,
		}
	}
}

/// Configures LDAP authentication and directory-backed administration.
///
/// Connection, bind, search, and attribute settings determine how users are
/// located and authenticated. Optional admin search settings identify directory
/// entries treated as server administrators.
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(filename = "tuwunel-example.toml", section = "global.ldap")]
pub struct LdapConfig {
	/// Whether to enable LDAP login.
	///
	/// reloadable: yes
	/// example: "true"
	#[serde(default)]
	pub enable: bool,

	/// URI of the LDAP server.
	///
	/// reloadable: yes
	/// example: "ldap://ldap.example.com:389"
	pub uri: Option<Url>,

	/// Root of the searches.
	///
	/// reloadable: yes
	/// example: "ou=users,dc=example,dc=org"
	///
	/// default:
	#[serde(default)]
	pub base_dn: String,

	/// Bind DN if anonymous search is not enabled.
	///
	/// You can use the variable `{username}` that will be replaced by the
	/// entered username. In such case, the password used to bind will be the
	/// one provided for the login and not the one given by
	/// `bind_password_file`. Beware: automatically granting admin rights will
	/// not work if you use this direct bind instead of a LDAP search.
	///
	/// reloadable: yes
	/// example: "cn=ldap-reader,dc=example,dc=org" or
	/// "cn={username},ou=users,dc=example,dc=org"
	///
	/// default: ""
	#[serde(default)]
	pub bind_dn: Option<String>,

	/// Path to a file on the system that contains the password for the
	/// `bind_dn`.
	///
	/// The server must be able to access the file, and it must not be empty.
	///
	/// reloadable: yes
	/// default: ""
	#[serde(default)]
	pub bind_password_file: Option<PathBuf>,

	/// Search filter to limit user searches.
	///
	/// You can use the variable `{username}` that will be replaced by the
	/// entered username for more complex filters.
	///
	/// reloadable: yes
	/// example: "(&(objectClass=person)(memberOf=matrix))"
	///
	/// default: "(objectClass=*)"
	#[serde(default = "default_ldap_search_filter")]
	pub filter: String,

	/// Attribute to use to uniquely identify the user.
	///
	/// reloadable: yes
	/// example: "uid" or "cn"
	///
	/// default: "uid"
	#[serde(default = "default_ldap_uid_attribute")]
	pub uid_attribute: String,

	/// Root of the searches for admin users.
	///
	/// Defaults to `base_dn` if empty.
	///
	/// reloadable: yes
	/// example: "ou=admins,dc=example,dc=org"
	///
	/// default:
	#[serde(default)]
	pub admin_base_dn: String,

	/// The LDAP search filter to find administrative users for tuwunel.
	///
	/// If left blank, administrative state must be configured manually for each
	/// user.
	///
	/// You can use the variable `{username}` that will be replaced by the
	/// entered username for more complex filters.
	///
	/// reloadable: yes
	/// example: "(objectClass=tuwunelAdmin)" or "(uid={username})"
	///
	/// default:
	#[serde(default)]
	pub admin_filter: String,
}

/// Configures authentication using JSON Web Tokens.
///
/// Key format, signature algorithm, and claim rules determine token validity.
/// Optional provisioning creates a local account for an otherwise valid token.
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(filename = "tuwunel-example.toml", section = "global.jwt")]
pub struct JwtConfig {
	/// Enable JWT logins
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub enable: bool,

	/// Validation key, also called 'secret' in Synapse config. The type of key
	/// can be configured in 'format', but defaults to the common HMAC which
	/// is a plaintext shared-secret, so you should keep this value private.
	///
	/// display: sensitive
	/// reloadable: yes
	/// default:
	#[serde(default, alias = "secret")]
	pub key: String,

	/// Format of the 'key': HMAC, B64HMAC, ECDSA, or EDDSA, case-insensitive.
	///
	/// HMAC is a plaintext shared secret, and B64HMAC (also spelled HMACB64)
	/// is the same secret base64-encoded in the standard alphabet with '='
	/// padding, for random binary secrets that cannot be pasted as text.
	/// ECDSA and EDDSA are PEM-encoded public keys, EDDSA for Ed25519.
	/// Startup and reload both refuse an unknown format while JWT is enabled.
	///
	/// reloadable: yes
	/// default: "HMAC"
	#[serde(default = "default_jwt_format")]
	pub format: String,

	/// Automatically create new user from a valid claim, otherwise access is
	/// denied for an unknown even with an authentic token.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub register_user: bool,

	/// Signature algorithm for validating tokens.
	///
	/// The name must suit the key format: HS256, HS384, or HS512 for HMAC
	/// and B64HMAC, ES256 or ES384 for ECDSA, and EdDSA for EDDSA.
	///
	/// reloadable: yes
	/// default: "HS256"
	#[serde(default = "default_jwt_algorithm")]
	pub algorithm: String,

	/// Optional audience claim list. The token must claim one or more values
	/// from this list when set.
	///
	/// reloadable: yes
	/// default: []
	#[serde(default)]
	pub audience: Vec<String>,

	/// Optional issuer claim list. The token must claim one or more values
	/// from this list when set.
	///
	/// reloadable: yes
	/// default: []
	#[serde(default)]
	pub issuer: Vec<String>,

	/// Require expiration claim in the token. This defaults to false for
	/// synapse migration compatibility.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub require_exp: bool,

	/// Require not-before claim in the token. This defaults to false for
	/// synapse migration compatibility.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub require_nbf: bool,

	/// Validate expiration time of the token when present. Whether or not it is
	/// required depends on require_exp, but when present this ensures the token
	/// is not used after a time.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub validate_exp: bool,

	/// Validate not-before time of the token when present. Whether or not it is
	/// required depends on require_nbf, but when present this ensures the token
	/// is not used before a time.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub validate_nbf: bool,

	/// Bypass validation for diagnostic/debug use only.
	///
	/// reloadable: yes
	/// default: true
	#[serde(default = "true_fn")]
	pub validate_signature: bool,
}

/// Configures request rate limits.
///
/// Each subsection limits one kind of request, such as password sign-in.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rate_limiting",
	ignore = "login"
)]
pub struct RateLimits {
	/// Limits password sign-in per account, in the `failed` and `account`
	/// subsections.
	// external structure; separate section
	#[serde(default)]
	pub login: LoginRateLimits,
}

/// Limits password sign-in per account on two axes.
///
/// Both mirror Synapse's `rc_login` limits of the same purpose and are keyed on
/// the account rather than the client address.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rate_limiting.login",
	ignore = "failed account"
)]
pub struct LoginRateLimits {
	/// Limits wrong passwords against one account.
	// external structure; separate section
	#[serde(default)]
	pub failed: LoginFailedRateLimit,

	/// Limits successful sign-ins to one account.
	// external structure; separate section
	#[serde(default)]
	pub account: LoginAccountRateLimit,
}

/// Limits wrong passwords against one account.
///
/// A name that matches no account counts as a wrong password, so the limit
/// does not reveal which accounts exist.
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rate_limiting.login.failed"
)]
pub struct LoginFailedRateLimit {
	/// Token-bucket refill rate (failures per second) for wrong passwords
	/// against one account.
	///
	/// Every password attempt takes a token before the password is checked,
	/// and only a wrong password keeps it; once the bucket is empty, even the
	/// owner's correct password is refused with `M_LIMIT_EXCEEDED`. Applies to
	/// `/login` (but not to the LDAP binds it makes), the built-in OIDC login
	/// page and password re-entry for sensitive actions (UIAA), keyed on the
	/// account. Mirrors Synapse's `rc_login.failed_attempts` and its default:
	/// `burst_count` failures, then about one attempt every six seconds, or
	/// some 14,700 a day.
	///
	/// `0` disables this limit rather than making a bucket that never refills.
	///
	/// reloadable: yes
	/// default: 0.17
	#[serde(default = "default_login_failed_per_second")]
	pub per_second: f64,

	/// Token-bucket depth (burst size) for wrong passwords against one
	/// account.
	///
	/// The number of failures allowed before `per_second` governs. `0`
	/// disables this limit, as does a `0` rate. The default is Synapse's
	/// `rc_login.failed_attempts.burst_count`.
	///
	/// reloadable: yes
	/// default: 3
	#[serde(default = "default_login_failed_burst_count")]
	pub burst_count: u32,
}

/// Limits successful sign-ins to one account.
///
/// Only a sign-in by a verified password takes a token, so wrong guesses never
/// lock the owner out on this axis.
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rate_limiting.login.account"
)]
pub struct LoginAccountRateLimit {
	/// Token-bucket refill rate (sign-ins per second) for successful password
	/// logins to one account.
	///
	/// Applies to `/login` with `m.login.password` (but not to the LDAP binds it
	/// makes) and to the built-in OIDC login page, keyed on the account rather
	/// than the client IP. Mirrors Synapse's `rc_login.account` and its default
	/// of `burst_count` sign-ins, then about one every five and a half minutes;
	/// once those are used up, a correct password is refused with
	/// `M_LIMIT_EXCEEDED`.
	///
	/// `0` disables this limit rather than making a bucket that never refills.
	///
	/// reloadable: yes
	/// default: 0.003
	#[serde(default = "default_login_account_per_second")]
	pub per_second: f64,

	/// Token-bucket depth (burst size) for successful password logins to one
	/// account.
	///
	/// The number of sign-ins allowed before `per_second` governs. `0` disables
	/// this limit, as does a `0` rate. The default is Synapse's
	/// `rc_login.account.burst_count`.
	///
	/// reloadable: yes
	/// default: 5
	#[serde(default = "default_login_account_burst_count")]
	pub burst_count: u32,
}

/// Configures outbound email verification through SMTP.
///
/// The connection URI and sender identify the relay and source mailbox.
/// Registration flags control which flows require a verified email address.
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(filename = "tuwunel-example.toml", section = "global.smtp")]
pub struct SmtpConfig {
	/// Connection URL for the outbound SMTP relay used to send email
	/// verification messages. Setting this enables the email subsystem;
	/// without it no mail is sent.
	///
	/// Use a `smtp://` URL for an unencrypted or STARTTLS connection and a
	/// `smtps://` URL for implicit TLS. Credentials and the host go inline:
	/// `smtps://user:pass@host:port`. The port defaults per scheme when
	/// omitted.
	///
	/// The userinfo component is URL-encoded, so an `@` inside the username
	/// must be written as `%40` (for example a login of `bot@example.com`
	/// becomes `smtps://bot%40example.com:pass@host:465`). Other reserved
	/// characters in the username or password are percent-encoded the same
	/// way.
	///
	/// display: uri-userinfo
	/// example: "smtps://user:pass@mail.example.com:465"
	pub connection_uri: Option<String>,

	/// The mailbox that outbound verification messages are sent from. Accepts
	/// either a bare address or a display-name form.
	///
	/// example: "Example <noreply@example.com>"
	pub sender: Option<String>,

	/// Require a verified email address to complete registration. When set,
	/// the registration flow does not finish until the user proves control of
	/// an email address.
	///
	/// default: false
	#[serde(default)]
	pub require_email_for_registration: bool,

	/// Require a verified email address when registering with a registration
	/// token. When set, token-based registration also demands a verified
	/// email address.
	///
	/// default: false
	#[serde(default)]
	pub require_email_for_token_registration: bool,
}

/// Configures one OpenID Connect identity provider.
///
/// Client credentials and endpoint discovery establish the upstream
/// authorization flow. Claim and trust settings control account mapping and
/// optional registration.
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "[global.identity_provider]"
)]
pub struct IdentityProvider {
	/// The brand-name of the service (e.g. Apple, Facebook, GitHub, GitLab,
	/// Google) or the software (e.g. keycloak, MAS) providing the identity.
	/// When a brand is recognized we apply certain defaults to this config
	/// for your convenience. For certain brands we apply essential internal
	/// workarounds specific to that provider; it is important to configure this
	/// field properly when a provider needs to be recognized (like GitHub for
	/// example).
	///
	/// Several configured providers can share the same brand name. It is not
	/// case-sensitive. As a convenience for common simple deployments we can
	/// identify this provider by brand in addition to the unique `client_id` if
	/// and only if there is a single provider for the brand; see notes for
	/// `client_id`.
	#[serde(deserialize_with = "utils::string::de::to_lowercase")]
	pub brand: String,

	/// The ID of your OAuth application which the provider generates upon
	/// registration. This ID then uniquely identifies this configuration
	/// instance itself, becoming the identity provider's ID and must be unique
	/// and remain unchanged.
	///
	/// As a convenience we also identify this config by `brand` if and only if
	/// there is a single provider configured for a `brand`. Note carefully that
	/// multiple providers configured with the same `brand` is not an error and
	/// this provider will simply not be found when querying by `brand`.
	pub client_id: String,

	/// Secret key the provider generated for you along with the `client_id`
	/// above. Unlike the `client_id`, the `client_secret` can be changed here
	/// whenever the provider regenerates one for you.
	///
	/// display: sensitive
	#[debug("{}", redacted_debug!(client_secret))]
	pub client_secret: Option<String>,

	/// Secret key to use, read from the file path specified.
	///
	/// Alternative to `client_secret` for deployments that prefer to keep the
	/// secret outside the config file. When both are configured `client_secret`
	/// is used and this field is ignored. The file is read at startup and on
	/// each OAuth exchange, must exist and must be non-empty; leading and
	/// trailing whitespace is trimmed. Under systemd the path must be visible
	/// to the service after sandboxing (`ReadWritePaths` / `ProtectHome`),
	/// typically by placing the file under `/etc/tuwunel/`.
	///
	/// example: "/etc/tuwunel/.client_secret"
	pub client_secret_file: Option<PathBuf>,

	/// Issuer URL the provider publishes for you. We have pre-supplied default
	/// values for some of the canonical public providers, making this field
	/// optional based on the `brand` set above. Otherwise it is required to
	/// find self-hosted providers. It must be identical to what is configured
	/// and expected by the provider and must never change because we associate
	/// identities to it. If the `/.well-known/openid-configuration` is not
	/// found behind this URL see `base_path` below as a workaround.
	pub issuer_url: Option<Url>,

	/// The callback URL configured when registering the OAuth application with
	/// the provider. Tuwunel's callback URL must be strictly formatted exactly
	/// as instructed. The URL host must point directly at the matrix server and
	/// use the following path:
	/// `/_matrix/client/unstable/login/sso/callback/<client_id>` where
	/// `<client_id>` is the same one configured for this provider above.
	pub callback_url: Option<Url>,

	/// When more than one identity_provider has been configured and
	/// `single_sso` is false and `sso_custom_providers_page` is false this will
	/// determine the behavior of the `/_matrix/client/v3/login/sso/redirect`
	/// endpoint (note the url lacks a trailing `client_id`).
	///
	/// When only one identity_provider is configured it will be interpreted
	/// as the default and this does not need to be set. Otherwise a default
	/// *must* be selected for some clients (e.g. fluffychat) to work properly
	/// when the above conditions require it. To operate out-of-the-box we
	/// default to one configured provider if none are explicitly default; a
	/// warning will be logged on startup for this condition.
	///
	/// (EXPERIMENTAL) Multiple providers can be set to default. All providers
	/// configured with this option set to `true` will associate with the same
	/// Matrix account when a client flows through
	/// `/_matrix/client/v3/login/sso/redirect`.
	///
	/// When a user authorizes any provider configured default, the flow will
	/// include all other providers configured default as well for association.
	/// NOTE: authorization must succeed for ALL default providers.
	#[serde(default)]
	pub default: bool,

	/// Optional display-name for this provider instance seen on the login page
	/// by users. It defaults to `brand`. When configuring multiple providers
	/// using the same `brand` this can be set to distinguish them.
	pub name: Option<String>,

	/// Optional icon for the provider. The canonical providers have a default
	/// icon based on the `brand` supplied above when this is not supplied. Note
	/// that it uses an MXC url which is curious in the auth-media era and may
	/// not be reliable.
	pub icon: Option<OwnedMxcUri>,

	/// Optional list of scopes to authorize.
	///
	/// An empty array sends `openid email profile`. The exception is
	/// `brand = "MAS"`, which sends only `openid`: MAS rejects `profile`, and
	/// its userinfo endpoint returns just `sub` and `username`. Set this to
	/// request a different subset. The user can further restrict scopes during
	/// their authorization.
	///
	/// default: []
	#[serde(default)]
	pub scope: BTreeSet<String>,

	/// Optional list of userinfo claims which shape and restrict the way we
	/// compute a Matrix UserId for new registrations. Reviewing Tuwunel's
	/// documentation will be necessary for a complete description in detail. An
	/// empty array imposes no restriction here, avoiding generated fallbacks as
	/// much as possible.
	///
	/// For simplicity we reserve a claim called "unique" which can be listed
	/// alone to ensure *only* generated ID's are used for registrations.
	///
	/// Note that listing the claim "sub" has special significance and will take
	/// precedence over all other claims, listed or unlisted. "sub" is not
	/// normally used to determine a UserId unless explicitly listed here.
	///
	/// As of now arbitrary claims cannot be listed here, we only recognize
	/// specific hard-coded claims.
	///
	/// default: []
	#[serde(default)]
	pub userid_claims: BTreeSet<String>,

	/// Trusted providers can cause username conflicts (i.e. account hijacking)
	/// but this is precisely how an existing matrix account can be associated
	/// with a provider. When this option is set to true, the way we compute a
	/// Matrix UserId from userinfo claims is inverted: we find the first
	/// matching user and grant access to it. Whereas by default, when set to
	/// false, we skip matching users and register the first available username;
	/// falling-back to random characters to avoid conflicts.
	///
	/// Only set this option to true for providers you self-host and control.
	/// Never set this option to true for the public providers such as GitHub,
	/// GitLab, etc.
	///
	/// Note that associating an existing user with an untrusted provider is
	/// still possible but only with the command '!admin query oauth associate'.
	///
	/// default: false
	#[serde(default)]
	pub trusted: bool,

	/// Setting this option to false will inhibit unique ID's from being
	/// generated as a last-resort when determining a UserId from a provider's
	/// claims. In the case of untrusted providers, when all provided claims
	/// conflict with existing user accounts, a unique fallback ID needs
	/// to be generated for registration to not be denied with an error.
	///
	/// Set this option to false if you operate a private server or a trusted
	/// identity provider where random UserId's are undesirable; the result of a
	/// misconfiguration or other issue where an error is warranted.
	///
	/// This option should be set to true for public servers or some users may
	/// never be able to register.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub unique_id_fallbacks: bool,

	/// Controls whether new user registration is possible from this provider.
	/// When this option is set to false, authorizations from this provider
	/// only affect existing users and will never result in a new registration
	/// when the claims fail to match any existing user (in the case of trusted
	/// providers) or an available username is found (in the case of untrusted
	/// providers).
	///
	/// When LDAP is enabled, a user found in the LDAP directory counts as an
	/// existing user and is still provisioned on first login, since the
	/// directory is the authoritative account store.
	///
	/// Setting this option to false is generally not useful unless there is
	/// an explicit reason to do so.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub registration: bool,

	/// Optional extra path components after the issuer_url leading to the
	/// location of the `.well-known` directory used for discovery. If the path
	/// starts with a slash it will be treated as absolute, meaning overwriting
	/// any path in the issuer_url. The path needs to end with a slash. This
	/// will be empty for specification-compliant providers.
	pub base_path: Option<String>,

	/// Overrides the `.well-known` location where the provider's openid
	/// configuration is found. It is very unlikely you will need to set this;
	/// available for developers or special purposes only.
	pub discovery_url: Option<Url>,

	/// Overrides the authorize URL requested during the grant phase. This is
	/// generally discovered or derived automatically, but may be required as a
	/// workaround for any non-standard or undiscoverable provider.
	pub authorization_url: Option<Url>,

	/// Overrides the access token URL; the same caveats apply as with the other
	/// URL overrides.
	pub token_url: Option<Url>,

	/// Overrides the revocation URL; the same caveats apply as with the other
	/// URL overrides.
	pub revocation_url: Option<Url>,

	/// Overrides the introspection URL; the same caveats apply as with the
	/// other URL overrides.
	pub introspection_url: Option<Url>,

	/// Overrides the userinfo URL; the same caveats apply as with the other URL
	/// overrides.
	pub userinfo_url: Option<Url>,

	/// Whether to perform discovery and adjust this provider's configuration
	/// accordingly. This defaults to true. When true, it is an error when
	/// discovery fails and authorizations will not be attempted to the
	/// provider.
	#[serde(default = "true_fn")]
	pub discovery: bool,

	/// The duration in seconds before a grant authorization session expires.
	///
	/// default: 300
	#[serde(default = "default_sso_grant_session_duration")]
	pub grant_session_duration: Option<u64>,

	/// Whether to check the redirect cookie during the callback. This is a
	/// security feature and should remain enabled. This is available for
	/// developers or deployments which cannot tolerate cookies and are willing
	/// to tolerate the risks.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub check_cookie: bool,

	/// Extra query parameters appended to every authorization request sent to
	/// the identity provider.
	///
	/// E.g. to force re-authentication even if IdP cookies are present:
	/// ```toml
	/// [[global.identity_provider]]
	/// extra_authorization_parameters = { prompt = "login" }
	/// ```
	///
	/// default: {}
	#[serde(default)]
	pub extra_authorization_parameters: BTreeMap<String, String>,

	/// Forward the MSC3824 `action` query parameter from the SSO redirect
	/// endpoints to this provider as an OpenID Connect `prompt` value.
	///
	/// When a client appends `action=register` to a `/login/sso/redirect`
	/// request the upstream authorization request carries `prompt=create`
	/// (the OpenID Connect "Initiating User Registration" extension) so the
	/// provider can present its registration screen. `action=login` is left
	/// unforwarded to avoid forcing a re-authentication, and a `prompt` set in
	/// `extra_authorization_parameters` still applies in that case. An
	/// action-derived `prompt` takes precedence over one configured there.
	///
	/// Leave this disabled unless the provider supports the `prompt=create`
	/// registration extension; a provider that does not may reject or ignore
	/// the request.
	///
	/// default: false
	#[serde(default)]
	pub forward_action_prompt: bool,
}

impl IdentityProvider {
	/// Returns the provider's stable identifier.
	///
	/// The identifier is the OAuth application's client ID. It is borrowed from
	/// this configuration without allocation.
	#[inline]
	#[must_use]
	pub fn id(&self) -> &str { self.client_id.as_str() }

	/// Loads the effective client secret.
	///
	/// An inline secret takes precedence over a configured secret file. File
	/// contents are read asynchronously and trimmed before being returned.
	pub async fn get_client_secret(&self) -> Result<String> {
		if let Some(client_secret) = &self.client_secret {
			return Ok(client_secret.clone());
		}

		let Some(client_secret_file) = &self.client_secret_file else {
			return Err!("No client secret or client secret file configured");
		};

		let client_secret = tokio::fs::read_to_string(client_secret_file).await?;

		Ok(client_secret.trim().to_owned())
	}
}

/// Returns the name shown to a user choosing between providers.
///
/// The configured name wins, and the brand stands in when none is set.
#[implement(IdentityProvider)]
#[inline]
#[must_use]
pub fn display_name(&self) -> &str { self.name.as_deref().unwrap_or(&self.brand) }

/// Selects the backend for a named media storage provider.
///
/// Local providers store objects beneath a filesystem path, while S3 providers
/// use a compatible object store. The default variant disables the entry.
#[derive(Clone, Debug, Default, Deserialize)]
pub enum StorageProvider {
	/// Selects a local filesystem backend.
	///
	/// The contained settings root object paths beneath a configured directory.
	/// Startup checks can require that directory to be usable.
	#[expect(non_camel_case_types)]
	local(StorageProviderLocal),

	/// Selects an S3-compatible object storage backend.
	///
	/// The boxed settings configure endpoint, credentials, encryption, and
	/// multipart uploads. Custom endpoints permit compatible non-AWS services.
	#[expect(non_camel_case_types)]
	#[serde(rename = "s3", alias = "S3")]
	s3(Box<StorageProviderS3>),

	/// Selects a Google Cloud Storage backend.
	///
	/// The boxed settings configure the bucket and object prefix. Credentials
	/// are resolved by the object-store client, which falls back to the
	/// instance metadata server when none are configured; that is what allows
	/// use from GKE Workload Identity without any exported key material.
	#[expect(non_camel_case_types)]
	#[serde(rename = "gcs", alias = "GCS")]
	gcs(Box<StorageProviderGcs>),

	/// Disables this storage provider entry.
	///
	/// This is the default when no backend variant is selected. It carries no
	/// backend settings.
	#[default]
	None,
}

/// Configures local filesystem object storage.
///
/// `base_path` prefixes every object path belonging to this provider. Remaining
/// options control directory creation, cleanup, and startup checks.
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.storage_provider.<ID>.local"
)]
pub struct StorageProviderLocal {
	/// Absolute path to this local filesystem storage provider. Technically the
	/// provider exists at the filesystem root, and the base_path is prefixed to
	/// all objects.
	#[serde(alias = "path")]
	pub base_path: String,

	/// Creates the directory on the local filesystem if missing. This is not
	/// recommended to prevent misconfigured environments and missing mounts
	/// from silently succeeding.
	#[serde(default)]
	pub create_if_missing: bool,

	/// Toggles the preservation of a directory after its last file contents are
	/// removed.
	#[serde(default = "true_fn")]
	pub delete_empty_directories: bool,

	/// Enables checks performed at startup determining the usability of the
	/// local directory. Failures will abort the server's startup.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub startup_check: bool,
}

/// Configures an S3-compatible object storage provider.
///
/// Bucket, endpoint, and credential fields identify the remote store.
/// Transport, encryption, multipart, and startup options tune how objects are
/// accessed.
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.storage_provider.<ID>.s3",
	section_aliases = "S3"
)]
pub struct StorageProviderS3 {
	/// Supply an s3 URL e.g. "s3://bucket/path". These URLs may contain one
	/// or all of `bucket`, `region`, and `path` . When not supplied, such
	/// additional items can be supplied below individually.
	pub url: Option<String>,

	/// The name of the S3 bucket. e.g. "bucketname-123456789-us-west-2-an".
	pub bucket: Option<String>,

	/// The region of the S3 bucket. e.g. "us-west-2".
	///
	/// default: "us-east-1"
	pub region: Option<String>,

	/// Your amazon IAM Key ID with access granted to this bucket.
	/// e.g. "ABCDEFG1X1ZZYYXXWWVV"
	#[debug("{}", redacted_debug!(key))]
	pub key: Option<String>,

	/// The secret key component which is approx 40 characters of base64.
	///
	/// default:
	/// display: sensitive
	#[serde(skip_serializing)]
	#[debug("{}", redacted_debug!(secret))]
	pub secret: Option<String>,

	/// Optional path prefix within the bucket where all our operations will
	/// take place.
	#[serde(alias = "path")]
	pub base_path: Option<String>,

	/// (expert use) Override the location of s3 applied after components of the
	/// parsed `url` (or when none set).
	pub endpoint: Option<String>,

	/// (expert use) Override this property useful for some self-hosted
	/// environments. By default it is derived when parsing the primary `url`.
	#[serde(default)]
	pub use_vhost_request: Option<bool>,

	/// (expert use) Alternative session-token authentication method.
	///
	/// display: sensitive
	/// default:
	#[serde(skip_serializing)]
	#[debug("{}", redacted_debug!(token))]
	pub token: Option<String>,

	/// (expert use) Associated SSE-KMS key material.
	///
	/// display: sensitive
	#[debug("{}", redacted_debug!(kms))]
	pub kms: Option<String>,

	/// (expert use) When configured for the bucket it should be reflected here.
	pub use_bucket_key: Option<bool>,

	/// (expert use) Threshold size for switching to Multi-part uploads. This is
	/// a quirk of the S3 protocol which requires us to use a different approach
	/// for "large" uploads. This value determines what a "large" upload is. The
	/// default value should be sufficient for most providers. The value is a
	/// parsed string allowing SI or IEC units for convenience.
	///
	/// default: "100 MiB"
	#[serde(default = "default_multipart_threshold")]
	pub multipart_threshold: ByteSize,

	/// (expert use) Size of each individual part within a Multi-part upload.
	/// Once an upload exceeds `multipart_threshold` the payload is split into
	/// parts of this size, each sent as a separate HTTP PUT. Smaller values
	/// keep individual requests under per-request timeouts on slow uplinks at
	/// the cost of more round-trips. S3 requires every part except the last
	/// to be at least 5 MiB. The value is a parsed string allowing SI or IEC
	/// units for convenience.
	///
	/// default: "10 MiB"
	#[serde(default = "default_multipart_part_size")]
	pub multipart_part_size: ByteSize,

	/// (developer use) Allows relaxing default requirement forcing HTTPS.
	///
	/// default: true
	#[serde(default = "some_true_fn")]
	pub use_https: Option<bool>,

	/// (developer_use) Allows skipping request header signatures (will be
	/// reejected by AWS).
	///
	/// default: true
	#[serde(default = "some_true_fn")]
	pub use_signatures: Option<bool>,

	/// (developer_use) Allows disabling request payload signatures.
	///
	/// default: true
	#[serde(default = "some_true_fn")]
	pub use_payload_signatures: Option<bool>,

	/// (developer use) Enables checks performed at startup such as pinging the
	/// provider. Failures are considered critical startup errors which abort
	/// startup. When set to false, faulty providers are only discovered with
	/// first use and will not be fatal errors.
	///
	/// Only set this to false if you expect a provider to be down at startup or
	/// for development/testing purposes; checks are disabled when the server
	/// is started in '--maintenance' mode.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub startup_check: bool,
}

/// Configures a Google Cloud Storage provider.
///
/// Only the bucket and an optional object prefix are required. Credentials are
/// resolved by the object-store client in this order: an explicit service
/// account key, an application-credentials file, then the instance metadata
/// server. On GKE the metadata-server fallback means Workload Identity works
/// with no credential settings at all and no exported key material.
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.storage_provider.<ID>.gcs",
	section_aliases = "GCS"
)]
pub struct StorageProviderGcs {
	/// The name of the GCS bucket, e.g. "my-media-bucket". Required; an unset
	/// bucket disables this provider.
	///
	/// A `gs://bucket/path` URL is intentionally not accepted here: the path
	/// component would not be applied as an object prefix. Set `bucket` and
	/// `base_path` separately instead.
	pub bucket: Option<StorageBucketName>,

	/// Optional path prefix within the bucket where all our operations will take
	/// place.
	#[serde(alias = "path")]
	pub base_path: Option<String>,

	/// (expert use) Path to a Google service account key file. Leave unset to
	/// resolve credentials from the environment, which on GCE/GKE ends at the
	/// instance metadata server; that is the keyless path and the recommended
	/// configuration. Setting this requires exported key material.
	pub service_account_path: Option<String>,

	/// (expert use) Path to an application default credentials file. Leave unset
	/// to resolve credentials from the environment. Only service-account and
	/// authorized-user documents are recognised, not every
	/// external-account/workload-identity-federation file format.
	pub application_credentials_path: Option<String>,

	/// (expert use) Threshold size for switching to multi-part uploads. This value
	/// determines what a "large" upload is. The value is a parsed string allowing
	/// SI or IEC units for convenience.
	///
	/// default: "100 MiB"
	#[serde(default = "default_multipart_threshold")]
	pub multipart_threshold: ByteSize,

	/// (expert use) Size of each individual part within a multi-part upload. Once
	/// an upload exceeds `multipart_threshold` the payload is split into parts of
	/// this size, each sent as a separate request. The value is a parsed string
	/// allowing SI or IEC units for convenience.
	///
	/// default: "10 MiB"
	#[serde(default = "default_multipart_part_size")]
	pub multipart_part_size: ByteSize,

	/// (developer use) Allows skipping request signatures.
	///
	/// default: true
	#[serde(default = "some_true_fn")]
	pub use_signatures: Option<bool>,

	/// (developer use) Enables checks performed at startup, such as probing the
	/// provider. Failures are considered critical startup errors and abort
	/// startup. When set to false, faulty providers are only discovered on first
	/// use and will not be fatal errors.
	///
	/// The check is a listing probe: it requires `storage.objects.list`, proves
	/// neither write nor delete permission, and passes on an empty bucket.
	///
	/// default: true
	#[serde(default = "true_fn")]
	pub startup_check: bool,
}

/// Defines one inline Matrix application service registration.
///
/// Tokens, namespaces, and protocol flags are converted to the Matrix
/// registration model. The enclosing config map supplies the registration ID
/// when `id` is empty.
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.appservice.<ID>",
	ignore = "users aliases rooms",
	hidden = "id"
)]
pub struct AppService {
	/// Identifies the application service registration.
	///
	/// An empty value is replaced with the enclosing config map key. An
	/// explicit value must match that key.
	#[serde(default)]
	pub id: String,

	/// The URL for the application service.
	///
	/// Optionally set to `null` if no traffic is required.
	///
	/// On Unix, use `unix:///absolute/path.sock` to connect over a Unix socket.
	///
	/// example: "unix:///run/mautrix-whatsapp/appservice.sock"
	pub url: Option<String>,

	/// A unique token for application services to use to authenticate requests
	/// to Homeservers.
	///
	/// default:
	/// display: sensitive
	pub as_token: String,

	/// A unique token for Homeservers to use to authenticate requests to
	/// application services.
	///
	/// default:
	/// display: sensitive
	pub hs_token: String,

	/// The localpart of the user associated with the application service.
	pub sender_localpart: Option<String>,

	/// Events which are sent from certain users.
	#[serde(default)]
	pub users: Vec<AppServiceNamespace>,

	/// Events which are sent in rooms with certain room aliases.
	#[serde(default)]
	pub aliases: Vec<AppServiceNamespace>,

	/// Events which are sent in rooms with certain room IDs.
	#[serde(default)]
	pub rooms: Vec<AppServiceNamespace>,

	/// Whether requests from masqueraded users are rate-limited.
	///
	/// The sender is excluded.
	#[serde(default)]
	pub rate_limited: bool,

	/// The external protocols which the application service provides (e.g.
	/// IRC).
	///
	/// default: []
	#[serde(default)]
	pub protocols: Vec<String>,

	/// Whether the application service wants to receive ephemeral data.
	///
	/// default: false
	#[serde(default)]
	pub receive_ephemeral: bool,

	/// Whether the application service wants to do device management, as part
	/// of MSC4190.
	///
	/// default: false
	#[serde(default)]
	pub device_management: bool,

	/// Whether the application service wants MSC3202 transaction extensions
	/// (device lists, one-time-key counts, and unused fallback key types).
	///
	/// The registration-file key is `org.matrix.msc3202`; this inline-config
	/// key is `msc3202_transaction_extensions`.
	///
	/// default: false
	#[serde(default)]
	pub msc3202_transaction_extensions: bool,
}

impl From<AppService> for ruma::api::appservice::Registration {
	fn from(conf: AppService) -> Self {
		use ruma::api::appservice::Namespaces;

		let sender_localpart = conf
			.sender_localpart
			.unwrap_or_else(|| conf.id.clone());

		Self {
			id: conf.id,
			url: conf.url,
			as_token: conf.as_token,
			hs_token: conf.hs_token,
			receive_ephemeral: conf.receive_ephemeral,
			device_management: conf.device_management,
			msc3202_transaction_extensions: conf.msc3202_transaction_extensions,
			protocols: conf.protocols.into(),
			rate_limited: conf.rate_limited.into(),
			sender_localpart,
			namespaces: Namespaces {
				users: conf.users.into_iter().map(Into::into).collect(),
				aliases: conf.aliases.into_iter().map(Into::into).collect(),
				rooms: conf.rooms.into_iter().map(Into::into).collect(),
			},
			keys_claims: false,
		}
	}
}

/// Defines one namespace claimed by an application service.
///
/// The regular expression selects users, aliases, or rooms according to the
/// list containing this value. `exclusive` controls whether the service owns
/// every matching identifier.
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "[global.appservice.<ID>.<users|rooms|aliases>]"
)]
pub struct AppServiceNamespace {
	/// Whether this application service has exclusive access to events within
	/// this namespace.
	#[serde(default)]
	pub exclusive: bool,

	/// A regular expression defining which values this namespace includes.
	pub regex: String,
}

impl From<AppServiceNamespace> for ruma::api::appservice::Namespace {
	fn from(conf: AppServiceNamespace) -> Self {
		Self {
			exclusive: conf.exclusive,
			regex: conf.regex,
		}
	}
}

/// Items matched here will not generate an "unknown to tuwunel" warning when
/// configured. This is important for environment variables which share the
/// `TUWUNEL_` prefix namespace but aren't config items;  match them here in
/// their split+lowercased format.
static KNOWN_KEYS: &[&str; 3] = &["^config$", "^runtime_[a-z0-9_]+$", "^appservice_keys_claims$"];

/// Items listed here generate a deprecation warning when configured.
static DEPRECATED_KEYS: &[&str; 11] = &[
	"appservice_keys_claims",
	"cache_capacity",
	"conduit_cache_capacity_modifier",
	"ldap.name_attribute",
	"max_concurrent_requests",
	"well_known_client",
	"well_known_server",
	"well_known_support_page",
	"well_known_support_role",
	"well_known_support_email",
	"well_known_support_mxid",
];

impl Config {
	/// Loads raw configuration from ordered file and environment sources.
	///
	/// Explicit paths follow config files selected through the supported
	/// environment variables, and later files override earlier files. Config
	/// values from the three environment namespaces take precedence over files.
	pub fn load<'a, I>(paths: I) -> Result<Figment>
	where
		I: Iterator<Item = &'a Path>,
	{
		let paths = Self::file_paths(paths);
		let config = Self::load_files(paths)?;

		Self::merge_environment(config)
	}
}

#[implement(Config)]
pub(crate) fn file_paths<'a, I>(paths: I) -> impl Iterator<Item = PathBuf>
where
	I: Iterator<Item = &'a Path>,
{
	[
		Env::var("CONDUIT_CONFIG"),
		Env::var("CONDUWUIT_CONFIG"),
		Env::var("TUWUNEL_CONFIG"),
	]
	.into_iter()
	.flatten()
	.map(PathBuf::from)
	.chain(paths.map(Path::to_path_buf))
}

#[implement(Config)]
pub(crate) fn load_files<I, P>(paths: I) -> Result<Figment>
where
	I: Iterator<Item = P>,
	P: Into<PathBuf>,
{
	let toml_files = paths.map(Into::into).collect_vec();

	let invalid_toml_files = toml_files
		.iter()
		.filter(|path| !path.exists())
		.map(|path| path.as_os_str())
		.collect_vec();

	if !invalid_toml_files.is_empty() {
		return Err!(
			"The following config files do not exist or have broken symlinks: \
			 {invalid_toml_files:?}"
		);
	}

	toml_files
		.iter()
		.try_fold(Figment::new(), |config, path| {
			Self::load_file(path).map(|file| config.merge(file))
		})
}

#[implement(Config)]
fn load_file(path: &Path) -> Result<Figment> {
	let provider = Toml::file(path);
	let profiles = Provider::data(&provider)?;
	let values = profiles.get(&Profile::Default);
	let headerless = values.is_some_and(|values| {
		values
			.values()
			.any(|value| value.as_dict().is_none())
	});

	let has_global = values.is_some_and(|values| values.contains_key("global"));

	if headerless && has_global {
		return Err!(
			"Configuration file mixes bare keys with a [global] profile: {}.",
			path.display()
		);
	}

	let provider = match headerless {
		| true => provider.profile(Profile::Global),
		| false => provider.nested(),
	};

	let config = Figment::new().merge(Self::normalize(provider)?);

	Ok(config)
}

/// Normalizes one configuration source before it is combined with others.
///
/// Legacy paths retain source metadata and follow the same precedence as
/// canonical paths. Conflicting spellings within one source are rejected.
/// The returned provider supplies its snapshot to one combination operation.
#[implement(Config)]
pub fn normalize<P: Provider>(provider: P) -> Result<impl Provider> {
	normalize_provider(provider).map_err(Into::into)
}

#[implement(Config)]
pub(crate) fn merge_environment(config: Figment) -> Result<Figment> {
	ENV_PREFIXES
		.into_iter()
		.try_fold(config, |config, prefix| {
			environment(prefix)
				.map(|provider| config.merge(provider))
				.map_err(Into::into)
		})
}

impl Config {
	/// Finalize config
	pub fn new(raw_config: &Figment) -> Result<Self> {
		assert_canonical(raw_config)?;
		let config = raw_config
			.extract::<Self>()
			.map_err(redact_error)
			.map_err(|e| err!("There was a problem with your configuration file: {e}"))?;

		Ok(with_unknown(config, raw_config))
	}

	/// Validates the complete configuration.
	///
	/// The startup checks emit warnings for deprecated or risky settings and
	/// reject invalid combinations. Reload-specific comparisons are performed
	/// by the configuration manager separately.
	pub fn check(&self) -> Result { check(self) }
}

fn with_unknown(mut config: Config, raw: &Figment) -> Config {
	config
		.catchall
		.extend(unknown(raw).map(|path| (path, IgnoredAny)));

	config
}

/// Argon2id cost parameters for hashing a new password.
///
/// The three settings are interdependent, so they travel as a unit rather than
/// being read one at a time. Verification never consults them: the cost of
/// checking a password comes from the stored hash.
#[implement(Config)]
#[inline]
#[must_use]
pub fn password_hash_cost(&self) -> Cost {
	Cost {
		m_cost: self.argon2_m_cost,
		t_cost: self.argon2_t_cost,
		p_cost: self.argon2_p_cost,
	}
}

/// Returns whether login screens list each identity provider by name.
///
/// `single_sso` and `sso_custom_providers_page` each replace that list with
/// one single sign-on entry.
#[implement(Config)]
#[inline]
#[must_use]
pub fn lists_identity_providers(&self) -> bool {
	!self.sso_custom_providers_page && !self.single_sso
}

impl TlsConfig {
	/// Returns the configured TLS certificate and key paths together.
	///
	/// A pair is returned only when both options are present. Startup
	/// validation rejects a configuration containing only one of the two
	/// paths.
	#[must_use]
	pub fn get_tls_cert_key(&self) -> Option<(&Path, &Path)> {
		let cert = self.certs.as_ref()?;

		let cert = Path::new(cert);

		let key = self.key.as_ref()?; // this cannot fail, aborts startup on cert.is_some ^ key.is_some

		let key = Path::new(key);

		Some((cert, key))
	}
}

impl Default for LoginFailedRateLimit {
	fn default() -> Self {
		Self {
			per_second: default_login_failed_per_second(),
			burst_count: default_login_failed_burst_count(),
		}
	}
}

impl Default for LoginAccountRateLimit {
	fn default() -> Self {
		Self {
			per_second: default_login_account_per_second(),
			burst_count: default_login_account_burst_count(),
		}
	}
}

fn true_fn() -> bool { true }

fn default_rendezvous_session_max_bytes() -> usize { 4096 }

fn default_rendezvous_session_ttl() -> u64 { 600 }

fn default_rendezvous_max_sessions() -> usize { 100 }

fn default_rendezvous_rc_per_second() -> u32 { 10 }

fn default_rendezvous_rc_burst_count() -> u32 { 20 }

// Synapse's `rc_login` defaults, taken unchanged:
// https://element-hq.github.io/synapse/latest/usage/configuration/config_documentation.html#rc_login
fn default_login_failed_per_second() -> f64 { 0.17 }

fn default_login_failed_burst_count() -> u32 { 3 }

fn default_login_account_per_second() -> f64 { 0.003 }

fn default_login_account_burst_count() -> u32 { 5 }

fn some_true_fn() -> Option<bool> { Some(true) }

#[cfg(test)]
fn default_server_name() -> OwnedServerName { ruma::owned_server_name!("localhost") }

fn default_database_path() -> PathBuf { "/var/lib/tuwunel".to_owned().into() }

fn default_max_remote_profile_fields() -> usize { 100 }

fn default_max_profile_fields_per_request() -> usize { 64 }

fn default_appservice_timeout() -> u64 { 35 }

fn default_appservice_idle_timeout() -> u64 { 300 }

fn default_pusher_idle_timeout() -> u64 { 15 }

fn default_tracing_flame_filter() -> String {
	cfg!(debug_assertions)
		.then_some("trace,h2=off")
		.unwrap_or("info")
		.to_owned()
}

fn default_jaeger_filter() -> String {
	cfg!(debug_assertions)
		.then_some("trace,h2=off")
		.unwrap_or("info")
		.to_owned()
}

fn default_tracing_flame_output_path() -> String { "./tracing.folded".to_owned() }

/// do debug logging by default for debug builds
#[must_use]
pub fn default_log() -> String {
	cfg!(debug_assertions)
		.then_some("debug")
		.unwrap_or("info")
		.to_owned()
}

/// Returns the default tracing span-event mode.
///
/// The value is `none`, which disables span lifecycle event emission. It is
/// used when `log_span_events` is omitted.
#[must_use]
pub fn default_log_span_events() -> String { "none".into() }

fn default_notification_push_path() -> String { "/_matrix/push/v1/notify".to_owned() }

fn default_openid_token_ttl() -> u64 { 60 * 60 }

fn default_login_token_ttl() -> u64 { 2 * 60 * 1000 }

fn default_turn_ttl() -> u64 { 60 * 60 * 24 }

fn default_presence_idle_timeout_s() -> u64 { 5 * 60 }

fn default_presence_offline_timeout_s() -> u64 { 30 * 60 }

fn default_typing_federation_timeout_s() -> u64 { 30 }

fn default_typing_client_timeout_min_s() -> u64 { 15 }

fn default_typing_client_timeout_max_s() -> u64 { 45 }

fn default_new_user_displayname_suffix() -> String { "💕".to_owned() }

fn default_argon2_m_cost() -> u32 { 19 * 1024 }

fn default_argon2_t_cost() -> u32 { 2 }

fn default_argon2_p_cost() -> u32 { 1 }

fn default_sentry_endpoint() -> Option<Url> {
	let url = "https://8994b1762a6a95af9502a7900edabc4c@o4509498990067712.ingest.us.sentry.io/4509498993213440"
		.try_into()
		.expect("default sentry url is invalid");

	Some(url)
}

fn default_sentry_traces_sample_rate() -> f32 { 0.15 }

fn default_sentry_filter() -> String { "info".to_owned() }

fn default_admin_log_capture() -> String {
	cfg!(debug_assertions)
		.then_some("debug")
		.unwrap_or("info")
		.to_owned()
}

fn default_admin_room_tag() -> String { "m.server_notice".to_owned() }

/// Preserves the administrative identity used before it was configurable.
///
/// Omitting the setting keeps existing installations on their original user.
fn default_server_user_localpart() -> ServerUserLocalpart { "conduit".into() }

fn default_admin_output_max_events() -> usize { 1 }

#[expect(clippy::as_conversions, clippy::cast_precision_loss)]
fn parallelism_scaled_f64(val: f64) -> f64 { val * (sys::available_parallelism() as f64) }

fn parallelism_scaled_u32(val: u32) -> u32 {
	let val = val
		.try_into()
		.expect("failed to cast u32 to usize");
	parallelism_scaled(val)
		.try_into()
		.unwrap_or(u32::MAX)
}

fn parallelism_scaled(val: usize) -> usize { val.saturating_mul(sys::available_parallelism()) }

fn default_ldap_search_filter() -> String { "(objectClass=*)".to_owned() }

fn default_ldap_uid_attribute() -> String { String::from("uid") }

fn default_jwt_algorithm() -> String { "HS256".to_owned() }

fn default_jwt_format() -> String { "HMAC".to_owned() }

fn default_client_sync_timeout_min() -> u64 { 5000 }

fn default_client_sync_timeout_default() -> u64 { 30000 }

fn default_client_sync_timeout_max() -> u64 { 90000 }

fn default_access_token_ttl() -> u64 { 604_800 }

fn default_refresh_token_reuse_grace() -> u64 { 15 }

fn default_one_time_key_limit() -> usize { 256 }

fn default_sso_grant_session_duration() -> Option<u64> { Some(300) }

fn default_multipart_threshold() -> ByteSize { ByteSize::mib(100) }

fn default_multipart_part_size() -> ByteSize { ByteSize::mib(10) }
