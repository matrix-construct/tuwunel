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
	collections::{BTreeMap, BTreeSet},
	path::{Path, PathBuf},
};

use bytesize::ByteSize;
use derive_more::Debug;
pub use figment::{Figment, value::Value as FigmentValue};
use figment::{
	Profile as FigmentProfile, Provider,
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
	ip_source::IpSource,
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
	Err, Result, err, implement, redacted_debug,
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

/// Limits password sign-in by account and client address.
///
/// Public login and authenticated password re-entry use separate attempt
/// budgets, so public guesses cannot consume the re-entry allowance.
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

/// Limits wrong passwords against one account from one client address.
///
/// A name that matches no account counts as a wrong password, so the limit
/// does not reveal which accounts exist.
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

/// Limits password attempts across accounts from one client address.
///
/// Both successful and failed attempts consume this budget before password
/// verification, limiting password spraying and verification work.
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
	/// Configure trusted client-address forwarding behind a proxy, or its
	/// users share the proxy's address budget.
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
