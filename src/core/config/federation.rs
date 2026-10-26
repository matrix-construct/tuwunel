//! Configures federation access and local state derivation.
//!
//! Transport, key lookup, history recovery, and bounded fanouts have
//! dedicated settings. Access controls apply to remote server
//! communication.

use regex::RegexSet;
use ruma::OwnedServerName;
use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;
use crate::matrix::MAX_PREV_EVENTS;

/// Configures federation access and local state derivation.
///
/// Transport, key lookup, history recovery, and bounded fanouts have
/// dedicated settings. Access controls apply to remote server
/// communication.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.federation",
	group = "true",
	ignore = "fanout history keys transport",
	hidden = "resolve_state_locally_shadow"
)]
pub struct Federation {
	/// Allows federation with other servers.
	///
	/// Disabling this after installation risks federation breakage and is not
	/// recommended, although the setting is not permanent.
	///
	/// legacy: allow_federation
	#[serde(default = "true_fn")]
	pub enable: bool,

	/// Derives incoming federation event state from locally held unresolved
	/// ancestry.
	///
	/// When previous events are stored but unresolved, we derive state locally
	/// instead of requesting `/state_ids` from the origin server. This
	/// requires the entire unresolved local ancestry to be present; every
	/// other case falls back to a federation state fetch. Disabling this
	/// restores the previous behavior of always fetching.
	///
	/// reloadable: yes
	/// legacy: resolve_state_locally
	#[serde(default = "true_fn")]
	pub resolve_state_locally: bool,

	/// Limits unresolved local events visited by one local state derivation
	/// before falling back to federation.
	///
	/// This bounds worst-case memory and latency in rooms with a large
	/// unresolved backlog. A value of 0 disables local derivation entirely.
	///
	/// reloadable: yes
	/// default: 256
	/// legacy: resolve_state_locally_max
	#[serde(default = "default_resolve_state_locally_max")]
	pub resolve_state_locally_max: usize,

	/// Retains the legacy compatibility switch for local state derivation.
	///
	/// A configured value of true disables local derivation and its memo
	/// shortcut. Use `federation.resolve_state_locally = false` in new
	/// configurations.
	///
	/// reloadable: yes
	/// display: hidden
	/// legacy: resolve_state_locally_shadow
	#[serde(default)]
	pub resolve_state_locally_shadow: bool,

	/// Allows federation requests to this server itself.
	///
	/// This exists mainly for development. Requests to ourselves are
	/// unintended in ordinary operation and very likely indicate a bug.
	///
	/// reloadable: yes
	/// legacy: federation_loopback
	#[serde(default)]
	pub loopback: bool,

	/// Blocks server names matching listed regular expressions from incoming
	/// and outgoing federation and client joins or remote invites.
	///
	/// These global access controls check room IDs, room aliases, sender
	/// server names, sender users' server names, inbound federation X-Matrix
	/// origins, and the outbound federation handler. This server's own name is
	/// always permitted and exempt from the list.
	///
	/// reloadable: yes
	/// example: ["badserver\.tld$", "badphrase", "19dollarfortnitecards"]
	///
	/// default: []
	/// legacy: forbidden_remote_server_names
	#[serde(default, with = "serde_regex")]
	pub forbidden_remote_server_names: RegexSet,

	/// Experimentally permits only server names matching listed regular
	/// expressions.
	///
	/// The behavior will change and the `_experimental` suffix will be removed
	/// in an upcoming release; the matching details otherwise follow
	/// `federation.forbidden_remote_server_names`. The list is empty and
	/// inactive by default; any entry activates it and denies nonmatches,
	/// while this server's own name is always permitted. The denylist still
	/// applies afterward, allowing a broad match such as "^([^.]+\.)+example\.com$"
	/// here and an exclusion such as "^bad\.example\.com$" there.
	///
	/// reloadable: yes
	/// example: ["badserver\.tld$", "badphrase", "19dollarfortnitecards"]
	///
	/// default: []
	/// legacy: allowed_remote_server_names_experimental
	#[serde(default, with = "serde_regex")]
	pub allowed_remote_server_names_experimental: RegexSet,

	/// Enforces
	/// [MSC4311](https://github.com/matrix-org/matrix-spec-proposals/pull/4311)
	/// create-event validation in federated invite and knock stripped state.
	///
	/// When enabled, an invite is rejected if its `m.room.create` event is
	/// missing, not a full PDU, bound to a different room, or fails signature
	/// checks, and such events are dropped from knock stripped state. When
	/// disabled, as by default, failures are logged but tolerated for
	/// ecosystem interoperability; a full PDU bound cryptographically to a
	/// different room is always rejected for room versions 12 and later. A
	/// full PDU in knock stripped state that fails validation is always
	/// omitted, so this setting governs only legacy stripped creates for
	/// knocks.
	///
	/// reloadable: yes
	/// legacy: enforce_stripped_state_pdu_validation
	#[serde(default)]
	pub enforce_stripped_state_pdu_validation: bool,

	/// Settings for federation fanout.
	///
	/// display: section
	#[serde(default)]
	pub fanout: Fanout,

	/// Settings for federation history.
	///
	/// display: section
	#[serde(default)]
	pub history: History,

	/// Settings for federation keys.
	///
	/// display: section
	#[serde(default)]
	pub keys: Keys,

	/// Settings for federation transport.
	///
	/// display: section
	#[serde(default)]
	pub transport: Transport,
}

/// Configures bounded federation fanouts.
///
/// Concurrency and request timeouts limit survey work. Destination limits
/// require confirmation for larger admin surveys.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.federation.fanout",
	group = "true"
)]
pub struct Fanout {
	/// Sets the timeout in seconds for each bounded federation fanout request.
	///
	/// This applies to admin surveys and migrated room fanouts. A server that
	/// exceeds the deadline is reported as timed out while other destinations
	/// continue. Keep this below `federation.transport.timeout` and above
	/// `federation.transport.keys_timeout`.
	///
	/// reloadable: yes
	/// default: 15
	/// legacy: feds_timeout
	#[serde(default = "default_feds_timeout")]
	pub timeout: u64,

	/// Limits concurrency for bounded federation fanouts.
	///
	/// This caps admin surveys and migrated room and key fanouts. Lower values
	/// trade sweep latency for load without dropping destinations. 0 selects
	/// the built-in bounded default.
	///
	/// reloadable: yes
	/// default: 0
	/// legacy: feds_max_width
	#[serde(default)]
	pub max_width: usize,

	/// Limits destinations contacted by an admin feds survey without
	/// confirmation.
	///
	/// A room exceeding this requires the command's confirmation flag. Higher
	/// values permit longer diagnostic commands on the serial admin worker.
	///
	/// reloadable: yes
	/// default: 2048
	/// legacy: feds_destination_limit
	#[serde(default = "default_destination_limit")]
	pub destination_limit: usize,
}

/// Configures federation history fetching and recovery.
///
/// Traversal, concurrency, and fetch-round limits bound recovery work.
/// Context fetching and previous-event waiting control when remote requests
/// begin.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.federation.history",
	group = "true"
)]
pub struct History {
	/// Limits prior events visited by one recovery traversal.
	///
	/// reloadable: yes
	/// default: 1024
	///
	/// Raising this limit increases recovery work; it is not a count of network
	/// requests.
	/// legacy: max_fetch_prev_events
	#[serde(default = "default_max_fetch_prev_events")]
	pub max_fetch_prev_events: u16,

	/// Limits simultaneous backward-extremity upgrades during incoming-event
	/// recovery.
	///
	/// Lower values reduce recovery load at the cost of latency. This does not
	/// change the protocol `prev_events` bound or
	/// `federation.history.max_fetch_prev_events`.
	///
	/// reloadable: yes
	/// default: 20
	/// legacy: prev_events_concurrency
	#[serde(default = "default_prev_events_concurrency")]
	pub prev_events_concurrency: u16,

	/// Limits how many milliseconds an incoming timeline event waits for
	/// missing previous events before federation fetching.
	///
	/// A gap closing within this window skips the fetch. The event-driven wait
	/// wakes as soon as those events arrive, so the limit is a ceiling on
	/// added latency rather than a fixed cost. Set to 0 to fetch immediately.
	///
	/// reloadable: yes
	/// default: 750
	/// legacy: fetch_prev_wait_ms
	#[serde(default = "default_fetch_prev_wait_ms")]
	pub fetch_prev_wait_ms: u64,

	/// Enables experimental federation fetching of missing base events for
	/// room context requests.
	///
	/// For `/_matrix/client/v3/rooms/{roomId}/context/{eventId}` requests
	/// whose event is not held locally, we fetch it from a room peer and
	/// persist it before responding instead of returning a 404. This requires
	/// `federation.enable` and has no effect when federation is disabled.
	/// Other on-demand federation fetch sites are gated separately.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: fetch_unreceived_contexts_over_federation
	#[serde(default)]
	pub fetch_unreceived_contexts: bool,

	/// Limits concurrent server contacts per round of a federation event
	/// fetch.
	///
	/// This tightens the built-in fanout profile of every fetch kind and never
	/// widens one. A value of 0 leaves the profiles unchanged.
	///
	/// reloadable: yes
	/// default: 0
	/// legacy: fetch_fanout_max_width
	#[serde(default)]
	pub fetch_fanout_max_width: usize,

	/// Limits staged rounds attempted by a federation event fetch before
	/// giving up.
	///
	/// This tightens the built-in round count of every fetch kind and never
	/// raises one. A value of 0 leaves the profiles unchanged.
	///
	/// reloadable: yes
	/// default: 0
	/// legacy: fetch_fanout_rounds
	#[serde(default)]
	pub fetch_fanout_rounds: usize,
}

/// Configures federation signing-key lookup.
///
/// Trusted notaries can supplement or replace origin requests according to
/// policy. Batch size and concurrency limits bound trusted-server queries.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.federation.keys",
	group = "true"
)]
pub struct Keys {
	#[expect(clippy::doc_link_with_quotes)]
	/// Selects trusted notary servers used to gather other servers' public
	/// keys.
	///
	/// We do not currently support inbound batched key requests, so this list
	/// should contain only Synapse servers.
	///
	/// reloadable: yes
	/// example: ["matrix.org", "tchncs.de"]
	///
	/// default: ["matrix.org"]
	/// legacy: trusted_servers
	#[serde(default = "default_trusted_servers")]
	pub trusted_servers: Vec<OwnedServerName>,

	/// Queries configured trusted key servers before the origin when enabled.
	///
	/// Querying the origin first offers the best security by reducing exposure
	/// to a compromised trusted server. Enabling this can maximize federation
	/// and join performance, but evaluate the other settings that query
	/// trusted servers first only under specific high-load conditions before
	/// enabling it globally.
	///
	/// reloadable: yes
	/// legacy: query_trusted_key_servers_first
	#[serde(default)]
	pub query_trusted_servers_first: bool,

	/// Queries configured trusted key servers before origins specifically
	/// during room joins.
	///
	/// This limits exposure to compromised trusted servers to joins, whose
	/// many origin-key lookups can cause significant delays; it defaults to
	/// true to mitigate unexpected delays. Set to false when minimizing that
	/// exposure matters more than latency. This setting is ignored when
	/// `federation.keys.query_trusted_servers_first` is true.
	///
	/// reloadable: yes
	/// legacy: query_trusted_key_servers_first_on_join
	#[serde(default = "true_fn")]
	pub query_trusted_servers_first_on_join: bool,

	/// Queries only trusted key servers and never the origin.
	///
	/// This is intended for clusters or custom deployments using
	/// `federation.keys.trusted_servers` as forwarding agents to cache and
	/// deduplicate requests. Notary servers do not act as forwarding agents by
	/// default, so enable this only with a full understanding of the
	/// deployment.
	///
	/// reloadable: yes
	/// legacy: only_query_trusted_key_servers
	#[serde(default)]
	pub only_query_trusted_servers: bool,

	/// Limits keys requested in each trusted-server batch query.
	///
	/// reloadable: yes
	/// default: 192
	/// legacy: trusted_server_batch_size
	#[serde(default = "default_trusted_server_batch_size")]
	pub trusted_server_batch_size: usize,

	/// Limits simultaneous request batches when querying a trusted server.
	///
	/// reloadable: yes
	/// default: 2
	/// legacy: trusted_server_batch_concurrency
	#[serde(default = "default_trusted_server_batch_concurrency")]
	pub trusted_server_batch_concurrency: usize,
}

/// Configures federation client transport.
///
/// Request and key-lookup timeouts bound remote operations. Idle settings
/// control connection reuse.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.federation.transport",
	group = "true"
)]
pub struct Transport {
	/// Sets the federation client read timeout in seconds.
	///
	/// This applies to each read from a remote server rather than the whole
	/// request, which remains bounded by `network.http.total_timeout`.
	///
	/// default: 25
	/// legacy: federation_timeout
	#[serde(default = "default_timeout")]
	pub timeout: u64,

	/// Sets the timeout in seconds for client-initiated federation key
	/// lookups.
	///
	/// This covers `/keys/query` and `/keys/claim` against remote servers and
	/// should be well below `federation.transport.timeout`, so an unresponsive
	/// server does not outlast the requesting client's send deadline.
	/// Exceeding this bound records a transient federation failure for the
	/// remote server, making subsequent lookups back off instead of blocking
	/// again.
	///
	/// default: 8
	/// legacy: federation_keys_timeout
	#[serde(default = "default_keys_timeout")]
	pub keys_timeout: u64,

	/// Sets the federation client idle connection pool timeout (seconds).
	///
	/// default: 25
	/// legacy: federation_idle_timeout
	#[serde(default = "default_idle_timeout")]
	pub idle_timeout: u64,

	/// Limits idle federation client connections per host, defaulting to one
	/// because an open connection can generally be reused.
	///
	/// default: 1
	/// legacy: federation_idle_per_host
	#[serde(default = "default_idle_per_host")]
	pub idle_per_host: u16,
}

impl Default for Federation {
	fn default() -> Self {
		Self {
			enable: true_fn(),
			resolve_state_locally: true_fn(),
			resolve_state_locally_max: default_resolve_state_locally_max(),
			resolve_state_locally_shadow: Default::default(),
			loopback: Default::default(),
			forbidden_remote_server_names: Default::default(),
			allowed_remote_server_names_experimental: Default::default(),
			enforce_stripped_state_pdu_validation: Default::default(),
			fanout: Fanout::default(),
			history: History::default(),
			keys: Keys::default(),
			transport: Transport::default(),
		}
	}
}

impl Default for Fanout {
	fn default() -> Self {
		Self {
			timeout: default_feds_timeout(),
			max_width: Default::default(),
			destination_limit: default_destination_limit(),
		}
	}
}

impl Default for History {
	fn default() -> Self {
		Self {
			max_fetch_prev_events: default_max_fetch_prev_events(),
			prev_events_concurrency: default_prev_events_concurrency(),
			fetch_prev_wait_ms: default_fetch_prev_wait_ms(),
			fetch_unreceived_contexts: Default::default(),
			fetch_fanout_max_width: Default::default(),
			fetch_fanout_rounds: Default::default(),
		}
	}
}

impl Default for Keys {
	fn default() -> Self {
		Self {
			trusted_servers: default_trusted_servers(),
			query_trusted_servers_first: Default::default(),
			query_trusted_servers_first_on_join: true_fn(),
			only_query_trusted_servers: Default::default(),
			trusted_server_batch_size: default_trusted_server_batch_size(),
			trusted_server_batch_concurrency: default_trusted_server_batch_concurrency(),
		}
	}
}

impl Default for Transport {
	fn default() -> Self {
		Self {
			timeout: default_timeout(),
			keys_timeout: default_keys_timeout(),
			idle_timeout: default_idle_timeout(),
			idle_per_host: default_idle_per_host(),
		}
	}
}

fn default_resolve_state_locally_max() -> usize { 256 }

fn default_feds_timeout() -> u64 { 15 }

fn default_destination_limit() -> usize { 2048 }

fn default_max_fetch_prev_events() -> u16 { 1024_u16 }

fn default_prev_events_concurrency() -> u16 {
	u16::try_from(MAX_PREV_EVENTS).expect("MAX_PREV_EVENTS exceeds u16")
}

fn default_fetch_prev_wait_ms() -> u64 { 750 }

fn default_trusted_servers() -> Vec<OwnedServerName> {
	vec![OwnedServerName::try_from("matrix.org").expect("valid ServerName")]
}

fn default_trusted_server_batch_size() -> usize { 192 }

fn default_trusted_server_batch_concurrency() -> usize { 2 }

fn default_timeout() -> u64 { 25 }

fn default_keys_timeout() -> u64 { 8 }

fn default_idle_timeout() -> u64 { 25 }

fn default_idle_per_host() -> u16 { 1 }
