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
#[cfg(feature = "jwt_login")]
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

pub mod authentication;
pub mod directory;
pub mod encryption;
pub mod profile;
pub mod registration;

pub mod admin;
pub mod appservice_client;
pub mod client;
pub mod compatibility;
pub mod configuration;
pub mod logging;
pub mod presence;
pub mod push;
pub mod read_receipts;
pub mod rendezvous;
pub mod telemetry;
pub mod tracing;
pub mod turn;
pub mod typing;

use std::{
	borrow::Cow,
	collections::{BTreeMap, BTreeSet},
	iter::once,
	path::{Path, PathBuf},
};

use bytesize::ByteSize;
use derive_more::Debug;
use figment::{
	Error as FigmentError, Profile as FigmentProfile, Provider, Source,
	providers::{Env, Format},
};
pub use figment::{Figment, value::Value as FigmentValue};
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
	admin::Admin,
	appservice_client::AppserviceClient,
	authentication::Authentication,
	cache::Cache,
	check::check,
	client::Client,
	compatibility::Compatibility,
	configuration::Configuration,
	database::Database,
	directory::Directory,
	encryption::Encryption,
	federation::Federation,
	http::Http,
	ip_source::{IpSource, UNIX_SOCKET_PEER},
	logging::Logging,
	manager::Manager,
	media::Media,
	network::Network,
	presence::Presence,
	profile::Profile,
	push::Push,
	read_receipts::ReadReceipts,
	regenerate::{
		Overwrite, RegenerateOptions, RegenerationSummary, example_config, regenerate_config,
		write_example_config,
	},
	registration::Registration,
	rendezvous::Rendezvous,
	rooms::Rooms,
	runtime::Runtime,
	sending::Sending,
	sources::Sources,
	telemetry::Telemetry,
	tracing::Tracing,
	turn::Turn,
	typing::Typing,
};
use self::{
	format::Toml,
	normalize::{
		assert_canonical, environment, normalize as normalize_provider, redact_error, unknown,
	},
	proxy::ProxyConfig,
};
use crate::{
	Err, Error, Result, err, implement, redacted_debug,
	utils::{
		self, BoolExt,
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

/// Names a storage provider section and holds its settings.
///
/// Configured sections are borrowed from the config; the built-in `media`
/// provider is built on demand and owned.
pub type ProviderSection<'a> = (&'a str, Cow<'a, StorageProvider>);

/// Global settings
///
/// Every setting lives under `[global]`. The first keys below name the
/// server and its storage; each `[global.<name>]` section further down is
/// optional and keeps its defaults when left out.
///
/// Before the first start, set `server_name` and check `database_path`.
/// The first start records `server_name` in the database, and a database
/// recorded under another name is refused, so changing the name later
/// means starting over with an empty database. `database_path` holds the
/// database and, by default, uploaded media.
///
/// Tips:
/// - No config file is read by default. Name one with `TUWUNEL_CONFIG`
///   (or `CONDUWUIT_CONFIG`, `CONDUIT_CONFIG`) or with `-c`/`--config`,
///   which is repeatable and loads after it; later files win. A file that
///   mixes bare keys with a `[global]` header is refused.
/// - Environment variables override files: `TUWUNEL_` plus the key path
///   with `__` between levels, so `database.backup.path` becomes
///   `TUWUNEL_DATABASE__BACKUP__PATH`. `-O key=value` overrides both.
/// - Keys marked `reloadable: yes` apply on `SIGUSR1` or
///   `!admin server reload-config`; others may need a restart. A reload
///   that changes `server_name` fails.
/// - Misspelled keys are ignored, not rejected; most log a warning, and
///   `configuration.error_on_unknown_opts = true` makes that fatal.
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
	          http network media federation rooms sending authentication directory encryption \
	          profile registration admin appservice_client client compatibility configuration \
	          logging presence push read_receipts rendezvous telemetry tracing turn typing",
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

	/// Configures direct TLS listeners.
	///
	/// Values are read from the separate `[global.tls]` section. Certificate
	/// and key paths must be supplied together before TLS is enabled.
	// external structure; separate section
	#[serde(default)]
	pub tls: TlsConfig,

	/// Restore this online database backup on startup, before the database is
	/// opened. The value is a backup ID as listed by `!admin server
	/// list-backups`, or 0 for the most recent backup. Set by the
	/// `--restore-backup` command line argument, and refused from a
	/// configuration file, where it would repeat the restore on every
	/// startup.
	pub database_restore_backup: Option<u32>,

	/// Configures Matrix discovery documents and related endpoints.
	///
	/// Values are read from the separate `[global.well_known]` section. Client,
	/// server, support, and MatrixRTC responses consume these settings.
	// external structure; separate section
	#[serde(default)]
	pub well_known: WellKnownConfig,

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

	/// Configures request rate limits, one `[global.rate_limiting]` subsection
	/// per kind of request.
	// external structure; separate section
	#[serde(default)]
	pub rate_limiting: RateLimits,

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

	/// Enables listener sockets; can be set to false to disable listening. This
	/// option is intended for developer/diagnostic purposes only.
	#[serde(default = "true_fn")]
	pub listening: bool,

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

	/// Settings for authentication.
	///
	/// display: section
	#[serde(default)]
	pub authentication: Authentication,

	/// Settings for directory.
	///
	/// display: section
	#[serde(default)]
	pub directory: Directory,

	/// Settings for encryption.
	///
	/// display: section
	#[serde(default)]
	pub encryption: Encryption,

	/// Settings for profile.
	///
	/// display: section
	#[serde(default)]
	pub profile: Profile,

	/// Settings for registration.
	///
	/// display: section
	#[serde(default)]
	pub registration: Registration,

	/// Settings for admin.
	///
	/// display: section
	#[serde(default)]
	pub admin: Admin,

	/// Settings for appservice client.
	///
	/// display: section
	#[serde(default)]
	pub appservice_client: AppserviceClient,

	/// Settings for client.
	///
	/// display: section
	#[serde(default)]
	pub client: Client,

	/// Settings for compatibility.
	///
	/// display: section
	#[serde(default)]
	pub compatibility: Compatibility,

	/// Settings for configuration.
	///
	/// display: section
	#[serde(default)]
	pub configuration: Configuration,

	/// Settings for logging.
	///
	/// display: section
	#[serde(default)]
	pub logging: Logging,

	/// Settings for presence.
	///
	/// display: section
	#[serde(default)]
	pub presence: Presence,

	/// Settings for push.
	///
	/// display: section
	#[serde(default)]
	pub push: Push,

	/// Settings for read receipts.
	///
	/// display: section
	#[serde(default)]
	pub read_receipts: ReadReceipts,

	/// Settings for rendezvous.
	///
	/// display: section
	#[serde(default)]
	pub rendezvous: Rendezvous,

	/// Settings for telemetry.
	///
	/// display: section
	#[serde(default)]
	pub telemetry: Telemetry,

	/// Settings for tracing.
	///
	/// display: section
	#[serde(default)]
	pub tracing: Tracing,

	/// Settings for turn.
	///
	/// display: section
	#[serde(default)]
	pub turn: Turn,

	/// Settings for typing.
	///
	/// display: section
	#[serde(default)]
	pub typing: Typing,
}

/// Direct TLS
///
/// Lets the server terminate TLS itself. Most deployments leave
/// `[global.tls]` unset and let a reverse proxy handle HTTPS and
/// certificates, in which case every TCP listener speaks plain HTTP. Use it
/// when nothing sits in front of the server, together with a public
/// `http.listen.address` and the `http.listen.port` values that clients and
/// other homeservers connect to.
///
/// Set `certs` to a PEM file holding the full certificate chain and `key`
/// to its PEM private key; setting only one of the two fails startup.
///
/// Minimal example:
///
/// ```toml
///     [global.tls]
///     certs = "/etc/tuwunel/tls/fullchain.pem"
///     key = "/etc/tuwunel/tls/privkey.pem"
/// ```
///
/// Tips:
/// - TLS covers every TCP listener, including sockets passed by systemd.
///   One port cannot serve TLS while another serves plain HTTP;
///   `dual_protocol` makes every port accept both. The Unix socket stays
///   plain HTTP.
/// - The files are read once at startup; restart after renewing the
///   certificate.
/// - Builds without the `direct_tls` feature (on by default) refuse to start
///   while `certs` and `key` are set.
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

/// Matrix discovery (well-known) responses
///
/// Sets what the server answers at `/.well-known/matrix/client`,
/// `/.well-known/matrix/server` and `/.well-known/matrix/support`. Clients
/// and other servers fetch these files over HTTPS from the `server_name`
/// host, so the settings take effect only when requests for that host reach
/// the server, directly or through a reverse proxy. An endpoint with nothing
/// configured answers 404.
///
/// Support contacts and policies have their own tables,
/// `[global.well_known.support_contact.<ID>]` and
/// `[global.well_known.support_policy.<ID>]`; with `support_page` they make
/// up `/.well-known/matrix/support`. `livekit_url` publishes the MatrixRTC
/// (Element Call) backend in the client response.
///
/// Example for `example.com` served from `matrix.example.com`:
///
/// ```toml
///     [global.well_known]
///     client = "https://matrix.example.com"
///     server = "matrix.example.com:443"
/// ```
///
/// Tips:
/// - Other features need `client`: startup fails when `smtp.connection_uri`
///   is set without it, and the built-in OIDC server (any
///   `[global.identity_provider]` entry or `authentication.oidc.native_auth`)
///   stays off with a warning.
#[expect(rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize, Default)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.well_known",
	ignore = "support_contact support_policy",
	hidden = "support_role support_email support_mxid support_pgp_key"
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

	/// Publishes a support web page URL at `/.well-known/matrix/support`.
	///
	/// It is served beside the support contacts and policies, and any one of
	/// the three makes the endpoint answer instead of returning 404.
	///
	/// reloadable: yes
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

/// Support policy document
///
/// Publishes one policy document, such as a privacy policy or terms of
/// service, in `/.well-known/matrix/support` (the
/// `org.matrix.msc4266.policies` field of MSC4266). These are informational;
/// to make new users accept documents at sign-up, use
/// `[global.registration_terms.<ID>]`.
///
/// - `<ID>`: the policy id published to clients, such as `privacy_policy`.
///   The id is not validated (the spec allows `0-9 a-z A-Z . _ ~ -`). Bare
///   keys may use `A-Z a-z 0-9 _ -`; anything else must be quoted
///   (`[global.well_known.support_policy."privacy.v2"]`), and an unquoted
///   dot splits the name into nested tables.
///
/// `version` and a
/// `[global.well_known.support_policy.<ID>.policy_translation.<LANG>]`
/// table are required; a policy table without them fails to load. Give
/// it at least one language.
///
/// Minimal example:
///
/// ```toml
///     [global.well_known.support_policy.privacy_policy]
///     version = "1.0"
///
///     [global.well_known.support_policy.privacy_policy.policy_translation.en]
///     name = "Privacy Policy"
///     url = "https://example.com/privacy"
/// ```
///
/// Tips:
/// - The translation table is `policy_translation` here but `translations`
///   under `[global.registration_terms.<ID>]`.
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

/// Support policy translation
///
/// One language version of a `[global.well_known.support_policy.<ID>]`
/// document: the title and link clients show for that language. Add one
/// table per language; `name` and `url` are both required.
///
/// - `<ID>`: the `<ID>` of the policy table this translation belongs to,
///   spelled (and quoted) the same way.
/// - `<LANG>`: the language code, such as `en`, `fr` or `en-US` (RFC 5646;
///   some clients use `en_US`). It is published as the key for this
///   translation next to the policy's `version`, so it must not be
///   `version`; it is not validated otherwise. Bare keys may use
///   `A-Z a-z 0-9 _ -`; anything else must be quoted, and an unquoted dot
///   splits the name into nested tables.
///
/// Example:
///
/// ```toml
///     [global.well_known.support_policy.privacy_policy.policy_translation.de]
///     name = "Datenschutzrichtlinie"
///     url = "https://example.com/privacy-de"
/// ```
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

/// Registration terms
///
/// Requires new users to accept policy documents, such as terms of service
/// or a privacy policy, when they register. Each table adds one document to
/// the `m.login.terms` stage of registration and to the built-in sign-up
/// page (`authentication.oidc.native_auth`). With none configured,
/// registration has no terms stage.
///
/// - `<ID>`: the policy id sent to clients, such as `terms_of_service`.
///   Checked at startup: 1 to 255 characters from `0-9 a-z A-Z . _ ~ -`.
///   Bare keys may use `A-Z a-z 0-9 _ -`, so an id with `.` or `~` must be
///   quoted (`[global.registration_terms."tos.v2"]`); an unquoted dot
///   splits the name into nested tables.
///
/// `version` and a `[global.registration_terms.<ID>.translations.<LANG>]`
/// table are required; give it at least one language.
///
/// Minimal example:
///
/// ```toml
///     [global.registration_terms.terms_of_service]
///     version = "1.0"
///
///     [global.registration_terms.terms_of_service.translations.en]
///     name = "Terms of Service"
///     url = "https://example.com/terms"
/// ```
///
/// Tips:
/// - A terms stage does not satisfy the open-registration startup check:
///   `registration.enable` still needs a registration token or a required
///   email stage.
/// - Only new accounts see the terms (appservice and guest registrations
///   skip them); changing `version` does not ask existing users again.
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

/// Registration terms translation
///
/// One language version of a `[global.registration_terms.<ID>]` document:
/// the title and link shown for that language. Every translation's `url`
/// is recorded as accepted in the new account's `m.accepted_terms` account
/// data.
///
/// - `<ID>`: the `<ID>` of the policy table this translation belongs to,
///   spelled (and quoted) the same way.
/// - `<LANG>`: the language code, such as `en`, `fr` or `en-US` (RFC 5646;
///   some clients use `en_US`). It is sent as the key for this translation
///   next to the policy's `version`, so it must not be `version`; it is not
///   validated otherwise. Bare keys may use `A-Z a-z 0-9 _ -`; anything
///   else must be quoted, and an unquoted dot splits the name into nested
///   tables.
///
/// `name` and `url` are required; a `url` whose scheme is not `http` or
/// `https` fails the startup check.
///
/// Tips:
/// - The built-in sign-up page (`authentication.oidc.native_auth`) links
///   one translation per policy: `en` when present, otherwise the first
///   `<LANG>` in sort order.
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

/// Support contact
///
/// Publishes one contact in the `contacts` list of
/// `/.well-known/matrix/support`, where users and other server admins look
/// for someone to reach. With no contact, `support_page` or support policy
/// configured, the endpoint answers 404.
///
/// - `<ID>`: your label for this contact. It is not published; contacts
///   are listed in `<ID>` order, and startup errors name the entry by it.
///   Bare keys may use `A-Z a-z 0-9 _ -`; anything else must be quoted
///   (`[global.well_known.support_contact."ops.team"]`), and an unquoted
///   dot splits the name into nested tables.
///
/// `role` is required, normally `m.role.admin` or `m.role.security`;
/// `m.role.moderator` and `m.role.dpo` are accepted but published under
/// their unstable MSC4121 and MSC4265 names. Startup fails unless at least
/// one of `email_address` or `matrix_id` is set.
///
/// Example:
///
/// ```toml
///     [global.well_known.support_contact.admin]
///     role = "m.role.admin"
///     email_address = "admin@example.com"
///     matrix_id = "@admin:example.com"
/// ```
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
	/// example: "@admin:example.com"
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

/// LDAP login
///
/// Checks password sign-ins against an LDAP directory and creates the local
/// account on first successful bind, regardless of `registration.enable`.
/// Off by default; needs the `ldap` build feature, which the default build
/// includes.
///
/// Set `enable` and `uri`. With both `bind_dn` and `bind_password_file`,
/// the server binds as that service account, then searches `base_dn` with
/// `filter`; leaving either unset searches anonymously. A `bind_dn`
/// containing `{username}` skips the search and binds directly as the user.
///
/// Minimal example (search with a service account):
///
/// ```toml
///     [global.ldap]
///     enable = true
///     uri = "ldaps://ldap.example.org:636"
///     base_dn = "ou=users,dc=example,dc=org"
///     bind_dn = "cn=ldap-reader,dc=example,dc=org"
///     bind_password_file = "/etc/tuwunel/.ldap_bind_password"
/// ```
///
/// Tips:
/// - A search with no match falls back to the local password, and two or
///   more matches refuse the login. Direct bind never falls back, and with a
///   nonempty `admin_filter` it revokes admin from every user who signs in.
/// - `authentication.login.with_password = false` turns LDAP login off, and
///   `authentication.emergency_password` is ignored while LDAP is enabled.
/// - Only `uri` is checked at startup, and only that it parses as a URL: a
///   missing `uri` or an unreachable directory shows up as failed logins.
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

/// JSON Web Token login
///
/// Accepts a signed JWT as proof of identity, both as the
/// `org.matrix.login.jwt` login type and as a user-interactive
/// authentication stage, for deployments where an external system mints
/// tokens for its users. The token's `sub` claim is the user's localpart on
/// this server, lowercased. Off by default.
///
/// Set `enable` and `key`. The default `format` and `algorithm` expect an
/// HMAC shared secret with HS256; an issuer signing with ECDSA or EdDSA needs
/// its PEM public key in `key` and a matching `format` and `algorithm`.
///
/// Minimal example:
///
/// ```toml
///     [global.jwt]
///     enable = true
///     key = "replace-with-a-long-random-secret"
/// ```
///
/// Tips:
/// - Startup and reload refuse an empty `key` or an unknown `format` while
///   enabled, but a `key` that does not decode or an unsuitable `algorithm`
///   fails only at login.
/// - With `register_user` (the default), a valid token for an unknown user
///   creates the account regardless of `registration.enable`.
/// - `validate_signature = false` has an effect only in debug builds.
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
	#[debug("{}", redacted_debug!(str key))]
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

	/// Expire the access token of a new JWT login and withhold a refresh token.
	///
	/// The access token lasts `authentication.access_token_ttl` seconds
	/// whatever the client asks for, so staying signed in takes a fresh JWT and
	/// the token issuer decides how long a login lasts. Off by default because
	/// a client that cannot obtain a fresh JWT would be signed out for good.
	/// A JWT login retires the refresh token its device holds, but a client
	/// that only refreshes never logs in, so remove devices holding a refresh
	/// token when enabling this. Startup and reload refuse a zero
	/// `authentication.access_token_ttl` while enabled.
	///
	/// reloadable: yes
	/// default: false
	#[serde(default)]
	pub expire_access_token: bool,

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

/// Rate limits
///
/// Groups request rate limits by kind of request. It holds
/// `[global.rate_limiting.login]` for password sign-in; the built-in OIDC
/// endpoints have a separate per-IP throttle,
/// `authentication.oidc.rc_per_second`.
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

/// Password sign-in limits
///
/// Three token buckets guard password checks, each in its own sub-section:
/// `[global.rate_limiting.login.failed]` counts wrong passwords per account
/// and client address, `[global.rate_limiting.login.address]` counts every
/// attempt per client address across accounts, and
/// `[global.rate_limiting.login.account]` counts successful sign-ins per
/// account from any address. They cover `m.login.password` at `/login` and
/// the built-in OIDC login page. Password re-entry for sensitive actions
/// (UIAA) has its own `failed` and `address` buckets with the same settings
/// and no `account` limit. A refused attempt gets `M_LIMIT_EXCEEDED`.
///
/// Tips:
/// - Behind a reverse proxy, set `http.ip_source` to the header it writes.
///   Without it, password checks use the TCP peer address: a proxy on the
///   same host (or the Unix socket) gets no per-address limit, and every
///   user behind a proxy on another host shares one address.
/// - Sign-ins that `[global.ldap]` decides by binding to the directory are
///   not counted; its fallback to the local password is.
/// - Buckets are kept in memory and reset on restart.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rate_limiting.login",
	ignore = "failed account address"
)]
pub struct LoginRateLimits {
	/// Limits wrong passwords against one account from one client address.
	// external structure; separate section
	#[serde(default)]
	pub failed: LoginFailedRateLimit,

	/// Limits successful sign-ins to one account.
	// external structure; separate section
	#[serde(default)]
	pub account: LoginAccountRateLimit,

	/// Limits password attempts across accounts from one client address.
	// external structure; separate section
	#[serde(default)]
	pub address: LoginAddressRateLimit,
}

/// Wrong-password limit
///
/// Throttles password guessing against one account from one client address.
/// Only wrong passwords, including names that match no account, use up the
/// allowance: `burst_count` at once, then one more every 1/`per_second`
/// seconds. Other addresses keep their own allowance for the same account.
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rate_limiting.login.failed"
)]
pub struct LoginFailedRateLimit {
	/// Token-bucket refill rate for wrong passwords per account and client
	/// address.
	///
	/// Every attempt reserves a token before verification; a correct password
	/// returns it, while a wrong password keeps it.
	/// An empty bucket refuses even a correct password from that address,
	/// without blocking the account from other addresses.
	/// IPv6 addresses share a bucket within each /64, and authenticated
	/// password re-entry (UIAA) uses separate buckets from public login.
	///
	/// `0` disables this limit rather than making a bucket that never refills.
	///
	/// reloadable: yes
	/// default: 0.17
	#[serde(default = "default_login_failed_per_second")]
	pub per_second: f64,

	/// Token-bucket depth for wrong passwords per account and client address.
	///
	/// The number of failures allowed before `per_second` governs.
	/// `0` disables this limit, as does a `0` rate.
	///
	/// reloadable: yes
	/// default: 3
	#[serde(default = "default_login_failed_burst_count")]
	pub burst_count: u32,
}

/// Per-address attempt limit
///
/// Caps all password attempts from one client address across every account,
/// successful or not, bounding password spraying and hashing work:
/// `burst_count` attempts at once, then one more every 1/`per_second`
/// seconds. Clients behind one NAT, one IPv6 /64, or a reverse proxy on
/// another host share a budget unless `http.ip_source` names a forwarding
/// header. Without one, a loopback peer (a local proxy or the Unix
/// socket) is not limited per address.
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rate_limiting.login.address"
)]
pub struct LoginAddressRateLimit {
	/// Token-bucket refill rate for password attempts per client address.
	///
	/// IPv6 addresses share a bucket within each /64; clients behind a shared
	/// address share its allowance.
	/// Public login and authenticated password re-entry use separate buckets.
	/// Unless `http.ip_source` names a forwarding header, a loopback peer such
	/// as a local proxy or the Unix socket is not limited per address; configure
	/// trusted forwarding behind a proxy to limit its clients by address.
	///
	/// `0` disables this limit rather than making a bucket that never refills.
	///
	/// reloadable: yes
	/// default: 1.0
	#[serde(default = "default_login_address_per_second")]
	pub per_second: f64,

	/// Token-bucket depth for password attempts per client address.
	///
	/// The number of attempts allowed before `per_second` governs.
	/// `0` disables this limit, as does a `0` rate.
	///
	/// reloadable: yes
	/// default: 20
	#[serde(default = "default_login_address_burst_count")]
	pub burst_count: u32,
}

/// Successful sign-in limit
///
/// Caps how often one account can sign in with a correct password, from any
/// address: `burst_count` sign-ins at once, then one more every
/// 1/`per_second` seconds. Wrong passwords never count, so guessing cannot
/// lock the owner out on this axis. A script that logs in repeatedly can
/// reach the default limit.
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

/// Outbound email (SMTP)
///
/// Sends verification mail so users can prove they own an email address:
/// at registration, when adding an address, and for password reset. With
/// no `connection_uri`, no mail is sent and those requests are refused.
///
/// `connection_uri` needs `sender` and `well_known.client` (verification
/// links point there), and the `require_email_*` keys need `connection_uri`;
/// otherwise startup fails.
///
/// Minimal example:
///
/// ```toml
///     [global.well_known]
///     client = "https://matrix.example.com"
///
///     [global.smtp]
///     connection_uri = "smtps://user:pass@mail.example.com:465"
///     sender = "Example <noreply@example.com>"
/// ```
///
/// Tips:
/// - `require_email_for_registration` with `connection_uri` adds a
///   verified-email stage to registration, which startup accepts in place
///   of the registration token that `registration.enable` otherwise needs.
/// - Verification links open `/_tuwunel/3pid/email/validate` under
///   `well_known.client`; the reverse proxy must forward that path.
/// - The built-in OIDC sign-up page cannot collect an email address, so it
///   refuses registration while an email is required.
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

	/// Rejects email domains matching these regular expressions during native
	/// registration and self-service email addition.
	///
	/// SSO registration and email addresses supplied by identity providers are
	/// exempt.
	///
	/// Patterns match only the canonical domain: lowercase with `ß` expanded to
	/// `ss`, without changing plus-addressing or dots. Anchor patterns to match
	/// whole domains, for example `^example[.]com$` or `(^|[.])example[.]com$`
	/// to include subdomains; an empty list imposes no domain exclusions.
	/// Existing bindings and password resets remain usable, while previously
	/// verified addresses are checked again before native registration or
	/// self-service addition.
	///
	/// reloadable: yes
	/// default: []
	#[serde(default, with = "serde_regex")]
	pub forbidden_email_domains: RegexSet,

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

/// Upstream identity providers (SSO)
///
/// Delegates sign-in to external OAuth 2.0 or OpenID Connect providers;
/// repeat the `[[global.identity_provider]]` header once per provider. Both
/// legacy `m.login.sso` and the built-in OIDC server
/// (`[global.authentication.oidc]`) sign users in through these entries.
///
/// Each entry needs `brand`, a unique `client_id`, and `client_secret` or
/// a readable, nonempty `client_secret_file`; startup refuses a repeated
/// `client_id`, a provider with neither secret key, and an unreadable or
/// empty `client_secret_file`. `issuer_url` is
/// required except for the `github`, `gitlab` and `google` brands.
/// Register `well_known.client` followed by
/// `/_matrix/client/unstable/login/sso/callback/<client_id>` as the redirect
/// URI at the provider; it is the default `callback_url`, so set that
/// yourself when `well_known.client` is unset.
///
/// Minimal example:
///
/// ```toml
///     [[global.identity_provider]]
///     brand = "keycloak"
///     client_id = "tuwunel"
///     client_secret_file = "/etc/tuwunel/.client_secret"
///     issuer_url = "https://sso.example.com/realms/example"
/// ```
///
/// Tips:
/// - With several providers, mark one `default = true`; otherwise the first
///   entry serves the bare SSO redirect, and startup warns unless
///   `authentication.login.sso_custom_providers_page` is set.
/// - Discovery runs on first use, not at startup: a missing `issuer_url`, or
///   one that differs from the issuer the provider reports, fails sign-in.
/// - New accounts from a provider follow its `registration` key, not
///   `registration.enable`.
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
	/// `authentication.login.single_sso` is false and
	/// `authentication.login.sso_custom_providers_page` is false this will
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

	/// Whether userinfo claims may grant access to an existing matching account.
	///
	/// By default we skip matching users and register the first available
	/// username, falling back to random characters to avoid conflicts; a trusted
	/// provider instead finds the first matching user and grants access to it,
	/// which is how an existing matrix account is associated with a provider and
	/// also how one can be hijacked. A trusted provider's `email` claim is also
	/// bound to a new account when it sends no `email_verified` claim, where an
	/// untrusted one must mark it verified. Only set this option to true for
	/// providers you self-host and control, never for public providers such as
	/// GitHub or GitLab. An existing user can still be associated with an
	/// untrusted provider, but only with the command '!admin query oauth
	/// associate'.
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
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
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

/// Local filesystem storage provider
///
/// Stores media files under a local directory. Unless replaced, a built-in
/// local provider named `media` keeps media in the `media` directory under
/// `database_path`; add a table here to move that directory or to add
/// another one.
///
/// - `<ID>`: your name for this provider. Media uses a provider only when
///   `media.storage_providers` (default `["media"]`) lists it;
///   `media.store_on_providers` and `media.import.conduit.source_provider`
///   name providers the same way. Listing an undefined id fails startup.
///   `media` is the built-in local provider, and a `media` entry replaces
///   it. Use one backend (`local`, `s3` or `gcs`) per `<ID>`. Bare keys may
///   use `A-Z a-z 0-9 _ -`; anything else must be quoted
///   (`[global.storage_provider."my.media".local]`), and an unquoted dot
///   splits the name into nested tables.
///
/// `base_path` is required; an empty value disables the entry. Unless
/// `create_if_missing` is set, the directory must already exist or startup
/// fails.
///
/// Example (moving the built-in `media` directory):
///
/// ```toml
///     [global.storage_provider.media.local]
///     base_path = "/srv/tuwunel/media"
/// ```
///
/// Tips:
/// - The built-in `media` provider creates its directory; an explicit
///   `media` entry does so only with `create_if_missing`.
/// - Providers are built once at startup, so a config reload that adds,
///   removes or changes a provider section is refused; restart instead.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
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

/// S3 storage provider
///
/// Stores media in an S3-compatible object store: AWS S3, or another
/// service reached through `endpoint`.
///
/// - `<ID>`: your name for this provider. Media uses a provider only when
///   `media.storage_providers` (default `["media"]`) lists it;
///   `media.store_on_providers` and `media.import.conduit.source_provider`
///   name providers the same way. Listing an undefined id fails startup.
///   `media` is the built-in local provider, and a `media` entry replaces
///   it. Use one backend (`local`, `s3` or `gcs`) per `<ID>`. Bare keys may
///   use `A-Z a-z 0-9 _ -`; anything else must be quoted
///   (`[global.storage_provider."my.media".s3]`), and an unquoted dot
///   splits the name into nested tables.
///
/// Set `url` or `bucket`; with neither, the entry is skipped, even when
/// `media.storage_providers` lists it. Skipping it is an error only when no
/// enabled provider is left to receive uploads. Keys left unset fall back
/// to the standard `AWS_*` environment variables.
///
/// Example:
///
/// ```toml
///     [global.storage_provider.s3_media.s3]
///     bucket = "my-media-bucket"
///     region = "us-west-2"
///     key = "CHANGE_ME"
///     secret = "CHANGE_ME"
/// ```
///
/// Tips:
/// - `media.storage_providers = ["media", "s3_media"]` with
///   `media.store_on_providers = ["s3_media"]` writes new media to S3 and
///   still reads older media from `media`. Without `store_on_providers`,
///   every upload goes to both (startup warns).
/// - The path in an `s3://bucket/path` `url` is not used as a prefix; set
///   `base_path` instead.
/// - Providers are built once at startup, so a config reload that adds,
///   removes or changes a provider section is refused; restart instead.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.storage_provider.<ID>.s3",
	section_aliases = "S3"
)]
pub struct StorageProviderS3 {
	/// Sets the bucket, and for some forms the region, from an S3 URL.
	///
	/// An `s3://bucket` URL supplies only the bucket, and its path is ignored,
	/// so set `base_path` for a prefix within the bucket. An AWS `https://` URL
	/// that names a region (`https://s3.<region>.amazonaws.com/<bucket>` or
	/// `https://<bucket>.s3.<region>.amazonaws.com`) supplies it too, a
	/// Cloudflare R2 URL sets it to `auto`, and values taken from the URL
	/// override `bucket` and `region`.
	///
	/// example: "s3://my-media-bucket"
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

	/// (expert use) Sets the S3 endpoint for self-hosted or other compatible
	/// services.
	///
	/// A dual-stack AWS or Cloudflare R2 `url` sets its own endpoint, which
	/// overrides this value.
	pub endpoint: Option<String>,

	/// (expert use) Turns virtual-hosted-style requests on or off, which some
	/// self-hosted services need.
	///
	/// A virtual-hosted AWS `url` (`https://<bucket>.s3.<region>.amazonaws.com`)
	/// turns them on regardless of this value.
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

/// Google Cloud Storage provider
///
/// Stores media in a Google Cloud Storage bucket. With no credential keys
/// set, credentials come from the environment and finally the instance
/// metadata server, so GKE Workload Identity needs no key file.
///
/// - `<ID>`: your name for this provider. Media uses a provider only when
///   `media.storage_providers` (default `["media"]`) lists it;
///   `media.store_on_providers` and `media.import.conduit.source_provider`
///   name providers the same way. Listing an undefined id fails startup.
///   `media` is the built-in local provider, and a `media` entry replaces
///   it. Use one backend (`local`, `s3` or `gcs`) per `<ID>`. Bare keys may
///   use `A-Z a-z 0-9 _ -`; anything else must be quoted
///   (`[global.storage_provider."my.media".gcs]`), and an unquoted dot
///   splits the name into nested tables.
///
/// `bucket` is the only required key; without it the entry is skipped,
/// even when `media.storage_providers` lists it. Skipping it is an error
/// only when no enabled provider is left to receive uploads.
///
/// Minimal example:
///
/// ```toml
///     [global.storage_provider.gcs_media.gcs]
///     bucket = "my-media-bucket"
/// ```
///
/// Tips:
/// - `media.storage_providers = ["media", "gcs_media"]` with
///   `media.store_on_providers = ["gcs_media"]` writes new media to GCS and
///   still reads older media from `media`. Without `store_on_providers`,
///   every upload goes to both (startup warns).
/// - Providers are built once at startup, so a config reload that adds,
///   removes or changes a provider section is refused; restart instead.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
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

/// Application services (bridges and bots)
///
/// Registers an appservice inline, instead of through a registration file
/// in `appservice_dir` or `!admin appservices register`. Add one table per
/// appservice, with its namespaces in `[[global.appservice.<ID>.users]]`,
/// `rooms` and `aliases` tables. Inline appservices load at startup; a
/// config reload does not apply changes here.
///
/// - `<ID>`: your name for this appservice. It becomes the registration
///   `id` used by `!admin appservices` commands (an explicit `id` key must
///   match it), and the bot user is `@<ID>:<server_name>` unless
///   `sender_localpart` is set. It must be unique across all appservices.
///   Lowercase letters, digits, `_` and `-` suit both a bare key and a
///   localpart; anything else must be quoted
///   (`[global.appservice."my.bridge"]`), and an unquoted dot splits the
///   name into nested tables.
///
/// `as_token` and `hs_token` are required and must match the bridge's
/// registration; `url` is where events are sent (omit it if none are
/// needed).
///
/// Example:
///
/// ```toml
///     [global.appservice.whatsapp]
///     url = "http://127.0.0.1:29318"
///     as_token = "CHANGE_ME_AS_TOKEN"
///     hs_token = "CHANGE_ME_HS_TOKEN"
/// ```
///
/// Tips:
/// - An `id` or `as_token` that another registration also uses (inline,
///   in `appservice_dir` or from the admin room) aborts appservice loading
///   at startup, and registrations after it are not loaded; unregister an
///   admin-room copy before moving it here. The admin commands can neither
///   register over nor unregister an inline appservice.
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
	#[debug("{}", redacted_debug!(str as_token))]
	pub as_token: String,

	/// A unique token for Homeservers to use to authenticate requests to
	/// application services.
	///
	/// default:
	/// display: sensitive
	#[debug("{}", redacted_debug!(str hs_token))]
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
			scopes: Vec::new(),
		}
	}
}

/// Appservice namespaces
///
/// Claims the user IDs, room aliases or room IDs an appservice manages.
/// Each `[[...]]` header appends one entry to the list it names, so repeat
/// the header for more patterns. The appservice may act only as users
/// matching `users` and create only aliases matching `aliases`; the server
/// sends it events from rooms that hold its users or match its `aliases` or
/// `rooms` entries.
///
/// - `<ID>`: the `<ID>` of the `[global.appservice.<ID>]` table this entry
///   belongs to, spelled (and quoted) the same way.
/// - `<users|rooms|aliases>`: one of the literal names `users` (user IDs
///   on this server), `aliases` (room aliases) or `rooms` (room IDs). Any
///   other name is ignored without a warning.
///
/// `regex` is required. Matching is unanchored (add `^` and `$`),
/// case-insensitive for `users` and `aliases`, case-sensitive for `rooms`.
/// Write the pattern as a single-quoted TOML string so backslashes stay
/// literal.
///
/// Example:
///
/// ```toml
///     [[global.appservice.whatsapp.users]]
///     exclusive = true
///     regex = '^@whatsapp_.*:example\.com$'
/// ```
///
/// Tips:
/// - `exclusive = true` stops ordinary users from registering matching user
///   IDs or creating matching aliases; on `rooms` entries it has no effect.
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

	// figment reads only `is_file()` paths and silently loads anything else as empty.
	let invalid_toml_files = toml_files
		.iter()
		.filter(|path| !path.is_file())
		.map(|path| path.as_os_str())
		.collect_vec();

	if !invalid_toml_files.is_empty() {
		return Err!(
			"The following config files do not exist, are not regular files, or have broken \
			 symlinks: {invalid_toml_files:?}"
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
	let values = profiles.get(&FigmentProfile::Default);
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
		| true => provider.profile(FigmentProfile::Global),
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
	/// Extracts the typed configuration from the merged raw sources.
	///
	/// A rejection is reported against the configuration file when one was
	/// loaded. With none loaded, the field detail is followed by a note that
	/// no file was selected, naming `--config` and `TUWUNEL_CONFIG`. Keys the
	/// schema does not know are recorded in `catchall`.
	pub fn new(raw_config: &Figment) -> Result<Self> {
		assert_canonical(raw_config)?;
		let config = raw_config
			.extract::<Self>()
			.map_err(redact_error)
			.map_err(|e| extraction_error(raw_config, &e))?;

		Ok(with_unknown(config, raw_config))
	}

	/// Validates the complete configuration.
	///
	/// The startup checks emit warnings for deprecated or risky settings and
	/// reject invalid combinations. Reload-specific comparisons are performed
	/// by the configuration manager separately.
	pub fn check(&self) -> Result { check(self) }
}

fn extraction_error(raw: &Figment, error: &FigmentError) -> Error {
	let from_file = raw
		.metadata()
		.any(|metadata| matches!(metadata.source, Some(Source::File(_))));

	match from_file {
		| true => err!("There was a problem with your configuration file: {error}"),
		| false => err!(
			"There was a problem with your configuration: {error} (no configuration file was \
			 loaded; pass one with --config or set TUWUNEL_CONFIG)"
		),
	}
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
		m_cost: self.authentication.argon2.m_cost,
		t_cost: self.authentication.argon2.t_cost,
		p_cost: self.authentication.argon2.p_cost,
	}
}

/// Returns whether login screens list each identity provider by name.
///
/// `authentication.login.single_sso` and
/// `authentication.login.sso_custom_providers_page` each replace that list with
/// one single sign-on entry.
#[implement(Config)]
#[inline]
#[must_use]
pub fn lists_identity_providers(&self) -> bool {
	self.authentication
		.login
		.sso_custom_providers_page
		.is_false()
		&& !self.authentication.login.single_sso
}

/// Returns whether new accounts may be registered with a password.
///
/// Password registration needs both `registration.enable` and
/// `authentication.login.with_password`, since a password account on a server
/// refusing password login could never sign in again. Registration through an
/// identity provider follows that provider's own `registration` option instead.
#[implement(Config)]
#[inline]
#[must_use]
pub fn allows_password_registration(&self) -> bool {
	self.registration.enable && self.authentication.login.with_password
}

/// Lists the storage provider sections the storage service builds from.
///
/// These are the configured sections plus the built-in local `media`
/// provider, which keeps media in the `media` directory under
/// `database_path` and is left out when a `media` section replaces it.
#[implement(Config)]
pub fn storage_provider_sections(&self) -> impl Iterator<Item = ProviderSection<'_>> {
	let builtin = once("media")
		.filter(|name| !self.storage_provider.contains_key(*name))
		.map(|name| {
			let base_path = self
				.database_path
				.join(name)
				.to_string_lossy()
				.into_owned();

			let provider = StorageProviderLocal {
				create_if_missing: true,
				base_path,
				..Default::default()
			};

			(name, Cow::Owned(StorageProvider::local(provider)))
		});

	self.storage_provider
		.iter()
		.map(|(name, provider)| (name.as_str(), Cow::Borrowed(provider)))
		.chain(builtin)
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

impl StorageProvider {
	/// Reports whether this entry selects a backend the storage service builds.
	///
	/// `None` is never enabled; a backend variant defers to that backend's own
	/// rule.
	#[must_use]
	pub fn is_enabled(&self) -> bool {
		match self {
			| Self::local(local) => local.is_enabled(),
			| Self::s3(s3) => s3.is_enabled(),
			| Self::gcs(gcs) => gcs.is_enabled(),
			| Self::None => false,
		}
	}
}

impl StorageProviderLocal {
	/// Reports whether this local provider is enabled.
	///
	/// An empty `base_path` disables the entry.
	#[inline]
	#[must_use]
	pub fn is_enabled(&self) -> bool { !self.base_path.is_empty() }
}

impl StorageProviderS3 {
	/// Reports whether this S3 provider is enabled.
	///
	/// Setting neither `url` nor `bucket` disables the entry.
	#[inline]
	#[must_use]
	pub fn is_enabled(&self) -> bool { self.url.is_some() || self.bucket.is_some() }
}

impl StorageProviderGcs {
	/// Reports whether this GCS provider is enabled.
	///
	/// An unset `bucket` disables the entry.
	#[inline]
	#[must_use]
	pub fn is_enabled(&self) -> bool { self.bucket.is_some() }
}

impl Default for LoginFailedRateLimit {
	fn default() -> Self {
		Self {
			per_second: default_login_failed_per_second(),
			burst_count: default_login_failed_burst_count(),
		}
	}
}

impl Default for LoginAddressRateLimit {
	fn default() -> Self {
		Self {
			per_second: default_login_address_per_second(),
			burst_count: default_login_address_burst_count(),
		}
	}
}

fn default_login_address_per_second() -> f64 { 1.0 }

fn default_login_address_burst_count() -> u32 { 20 }

impl Default for LoginAccountRateLimit {
	fn default() -> Self {
		Self {
			per_second: default_login_account_per_second(),
			burst_count: default_login_account_burst_count(),
		}
	}
}

fn true_fn() -> bool { true }

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

fn default_sso_grant_session_duration() -> Option<u64> { Some(300) }

fn default_multipart_threshold() -> ByteSize { ByteSize::mib(100) }

fn default_multipart_part_size() -> ByteSize { ByteSize::mib(10) }
