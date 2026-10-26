//! Configures cache capacities and space hierarchy retention.
//!
//! Capacity settings cover RocksDB block caches and the in-memory state cache.
//! Space hierarchy settings bound how long cached room summaries remain valid.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::parallelism_scaled_u32;

/// Controls cache capacities and room summary retention.
///
/// Individual capacities select the size of each cache, while a shared modifier
/// scales their memory use. Space hierarchy settings control summary lifetimes.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.cache",
	group = "true",
	ignore = "space_hierarchy"
)]
pub struct Cache {
	/// Multiplies cache capacities by a floating-point factor.
	///
	/// Increase this value to use spare memory for performance, or reduce it
	/// when memory is limited. Individual capacities such as
	/// `cache.auth_chain_capacity` scale with the CPU core count by default.
	///
	/// default: 1.0
	/// legacy: cache_capacity_modifier conduit_cache_capacity_modifier
	#[serde(
		default = "default_capacity_modifier",
		alias = "conduit_cache_capacity_modifier"
	)]
	pub capacity_modifier: f64,

	/// Maximum number of entries in the RocksDB block cache shared by the
	/// `pduid_pdu` and `eventid_outlierpdu` column families: a PDU's full
	/// body, keyed by its PDU ID, and the same body keyed by event ID when
	/// the PDU is an outlier.
	///
	/// This is an entry count, not a byte size. The cache's actual capacity
	/// in bytes is this value multiplied by an internal per-column-family
	/// key+value size estimate, then multiplied by `cache.capacity_modifier`.
	/// Scaled by your CPU core count by default; see
	/// `cache.capacity_modifier` to scale this along with the other
	/// individual LRU caches at once.
	///
	/// default: varies by system
	/// legacy: pdu_cache_capacity
	#[serde(default = "default_pdu_capacity")]
	pub pdu_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache for the
	/// `authchainkey_authchain` column family: a room event's full auth
	/// chain, keyed by the set of events the chain was derived from.
	///
	/// Same entry-count semantics as `cache.pdu_capacity` above; see there
	/// for how this becomes a byte capacity and how
	/// `cache.capacity_modifier` applies.
	///
	/// default: varies by system
	/// legacy: auth_chain_cache_capacity
	#[serde(default = "default_auth_chain_capacity")]
	pub auth_chain_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache for the
	/// `shorteventid_eventid` column family: an event's full event ID,
	/// looked up from its short event ID.
	///
	/// Same entry-count semantics as `cache.pdu_capacity`.
	///
	/// default: varies by system
	/// legacy: shorteventid_cache_capacity
	#[serde(default = "default_shorteventid_capacity")]
	pub shorteventid_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache for the
	/// `eventid_shorteventid` column family: an event's short event ID,
	/// looked up from its full event ID.
	///
	/// This is the reverse lookup of `cache.shorteventid_capacity`.
	/// Same entry-count semantics as `cache.pdu_capacity`.
	///
	/// default: varies by system
	/// legacy: eventidshort_cache_capacity
	#[serde(default = "default_eventidshort_capacity")]
	pub eventidshort_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache for the
	/// `eventid_pduid` column family: an event's PDU ID, looked up from its
	/// full event ID.
	///
	/// Same entry-count semantics as `cache.pdu_capacity`.
	///
	/// default: varies by system
	/// legacy: eventid_pdu_cache_capacity
	#[serde(default = "default_eventid_pdu_capacity")]
	pub eventid_pdu_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache for the
	/// `eventid_backoff` column family: the recent fetch, auth, upgrade, and
	/// delivery outcomes an event is rate-gated against, keyed by federation
	/// step, event ID, and time bucket.
	///
	/// Same entry-count semantics as `cache.pdu_capacity`. A server working
	/// through a large missing-ancestry gap reads this column heavily.
	///
	/// default: varies by system
	/// legacy: eventid_backoff_cache_capacity
	#[serde(default = "default_eventid_backoff_capacity")]
	pub eventid_backoff_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache for the
	/// `shortstatekey_statekey` column family: a state event's full state
	/// key, looked up from its short state key.
	///
	/// Same entry-count semantics as `cache.pdu_capacity`.
	///
	/// default: varies by system
	/// legacy: shortstatekey_cache_capacity
	#[serde(default = "default_shortstatekey_capacity")]
	pub shortstatekey_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache for the
	/// `statekey_shortstatekey` column family: a state event's short state
	/// key, looked up from its full state key.
	///
	/// This is the reverse lookup of `cache.shortstatekey_capacity`.
	/// Same entry-count semantics as `cache.pdu_capacity`.
	///
	/// default: varies by system
	/// legacy: statekeyshort_cache_capacity
	#[serde(default = "default_statekeyshort_capacity")]
	pub statekeyshort_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache for the
	/// `servernameevent_data` column family: outbound federation events
	/// (PDUs and EDUs) queued for delivery, keyed by destination server
	/// name.
	///
	/// Same entry-count semantics as `cache.pdu_capacity`.
	///
	/// default: varies by system
	/// legacy: servernameevent_data_cache_capacity
	#[serde(default = "default_servernameevent_data_capacity")]
	pub servernameevent_data_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache for the
	/// `mediaid_lazycontent` column family: preview images staged by the URL
	/// preview fetcher, keyed by media ID, until a download promotes the row.
	///
	/// Same entry-count semantics as `cache.pdu_capacity`, against a modal
	/// 256 KiB entry. Staged rows are read once and deleted at promotion, so
	/// this mostly holds index blocks, and a single outsized preview can
	/// exceed the whole pool.
	///
	/// default: 128
	/// legacy: mediaid_lazycontent_cache_capacity
	#[serde(default = "default_mediaid_lazycontent_capacity")]
	pub mediaid_lazycontent_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache pool shared by the
	/// `servername_destination` and `servername_override` column families: a
	/// remote server's resolved federation destination, keyed by server name,
	/// and the address override for a resolved hostname.
	///
	/// Same entry-count semantics as `cache.pdu_capacity`, counted across the
	/// pool rather than per column.
	///
	/// default: varies by system
	/// legacy: resolver_cache_capacity
	#[serde(default = "default_resolver_capacity")]
	pub resolver_capacity: u32,

	/// Maximum number of entries in the RocksDB block cache for the
	/// `servername_status` column family: a remote server's recent
	/// reachability outcome, keyed by server name and time bucket.
	///
	/// Same entry-count semantics as `cache.pdu_capacity`.
	///
	/// default: varies by system
	/// legacy: servername_status_cache_capacity
	#[serde(default = "default_servername_status_capacity")]
	pub servername_status_capacity: u32,

	/// Maximum number of entries in the in-memory LRU cache of decompressed
	/// room state (a list of short state-info entries per state hash), used
	/// by the state compressor to avoid re-walking
	/// `shortstatehash_statediff` on every lookup.
	///
	/// Unlike the other caches on this page, this one is not backed by
	/// RocksDB: it is a plain in-process cache sized directly in entries,
	/// with no per-entry byte-size conversion. `cache.capacity_modifier`
	/// still applies to it.
	///
	/// default: varies by system
	/// legacy: stateinfo_cache_capacity
	#[serde(default = "default_stateinfo_capacity")]
	pub stateinfo_capacity: u32,

	/// Settings for cache space hierarchy.
	///
	/// display: section
	#[serde(default)]
	pub space_hierarchy: SpaceHierarchy,
}

/// Bounds the lifetime of cached space hierarchy summaries.
///
/// Minimum and maximum lifetimes are measured in seconds. Both limits can be
/// changed when the configuration is reloaded.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.cache.space_hierarchy",
	group = "true"
)]
pub struct SpaceHierarchy {
	/// Minimum time-to-live in seconds for room summary entries in the spaces
	/// cache.
	///
	/// reloadable: yes
	/// default: 10800
	/// legacy: spacehierarchy_cache_ttl_min
	#[serde(default = "default_ttl_min")]
	pub ttl_min: u64,

	/// Maximum time-to-live in seconds for room summary entries in the spaces
	/// cache.
	///
	/// reloadable: yes
	/// default: 64800
	/// legacy: spacehierarchy_cache_ttl_max
	#[serde(default = "default_ttl_max")]
	pub ttl_max: u64,
}

impl Default for Cache {
	fn default() -> Self {
		Self {
			capacity_modifier: default_capacity_modifier(),
			pdu_capacity: default_pdu_capacity(),
			auth_chain_capacity: default_auth_chain_capacity(),
			shorteventid_capacity: default_shorteventid_capacity(),
			eventidshort_capacity: default_eventidshort_capacity(),
			eventid_pdu_capacity: default_eventid_pdu_capacity(),
			eventid_backoff_capacity: default_eventid_backoff_capacity(),
			shortstatekey_capacity: default_shortstatekey_capacity(),
			statekeyshort_capacity: default_statekeyshort_capacity(),
			servernameevent_data_capacity: default_servernameevent_data_capacity(),
			mediaid_lazycontent_capacity: default_mediaid_lazycontent_capacity(),
			resolver_capacity: default_resolver_capacity(),
			servername_status_capacity: default_servername_status_capacity(),
			stateinfo_capacity: default_stateinfo_capacity(),
			space_hierarchy: SpaceHierarchy::default(),
		}
	}
}

impl Default for SpaceHierarchy {
	fn default() -> Self {
		Self {
			ttl_min: default_ttl_min(),
			ttl_max: default_ttl_max(),
		}
	}
}

fn default_capacity_modifier() -> f64 { 1.0 }

fn default_pdu_capacity() -> u32 { parallelism_scaled_u32(10_000).saturating_add(100_000) }

fn default_auth_chain_capacity() -> u32 {
	parallelism_scaled_u32(250_000).saturating_add(750_000)
}

fn default_shorteventid_capacity() -> u32 {
	parallelism_scaled_u32(200_000).saturating_add(400_000)
}

fn default_eventidshort_capacity() -> u32 {
	parallelism_scaled_u32(100_000).saturating_add(400_000)
}

fn default_eventid_pdu_capacity() -> u32 {
	parallelism_scaled_u32(100_000).saturating_add(400_000)
}

fn default_eventid_backoff_capacity() -> u32 {
	parallelism_scaled_u32(4_000).saturating_add(200_000)
}

fn default_shortstatekey_capacity() -> u32 {
	parallelism_scaled_u32(4_000).saturating_add(40_000)
}

fn default_statekeyshort_capacity() -> u32 {
	parallelism_scaled_u32(4_000).saturating_add(40_000)
}

fn default_servernameevent_data_capacity() -> u32 {
	parallelism_scaled_u32(50_000).saturating_add(200_000)
}

fn default_mediaid_lazycontent_capacity() -> u32 { 128 }

fn default_resolver_capacity() -> u32 { parallelism_scaled_u32(10_000).saturating_add(100_000) }

fn default_servername_status_capacity() -> u32 {
	parallelism_scaled_u32(15_000).saturating_add(200_000)
}

fn default_stateinfo_capacity() -> u32 { parallelism_scaled_u32(100) }

fn default_ttl_min() -> u64 { 60 * 60 * 3 }

fn default_ttl_max() -> u64 { 60 * 60 * 18 }
