//! Configures outbound discovery, DNS, and HTTP clients.
//!
//! Discovery settings bound federation well-known requests. DNS settings
//! control resolver behavior, while HTTP settings govern outbound clients.

use regex::RegexSet;
use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::{deserialize_bytesize_usize, true_fn};

/// Configures outbound networking and name resolution.
///
/// Discovery settings bound federation well-known requests. DNS and HTTP
/// client settings control resolution, request limits, and connection
/// behavior.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.network",
	group = "true",
	ignore = "discovery dns http"
)]
pub struct Network {
	/// Settings for network discovery.
	///
	/// display: section
	#[serde(default)]
	pub discovery: Discovery,

	/// Settings for network dns.
	///
	/// display: section
	#[serde(default)]
	pub dns: Dns,

	/// Settings for network http.
	///
	/// display: section
	#[serde(default)]
	pub http: Http,
}

/// Bounds federation well-known discovery requests.
///
/// Connection and request timeouts are measured in seconds. They apply to
/// HTTP discovery of federation destinations.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.network.discovery",
	group = "true"
)]
pub struct Discovery {
	/// Sets the federation well-known resolution connection timeout in
	/// seconds.
	///
	/// default: 6
	/// legacy: well_known_conn_timeout
	#[serde(default = "default_well_known_conn_timeout")]
	pub conn_timeout: u64,

	/// Sets the federation HTTP well-known resolution request timeout in
	/// seconds.
	///
	/// default: 10
	/// legacy: well_known_timeout
	#[serde(default = "default_well_known_timeout")]
	pub timeout: u64,
}

/// Configures DNS resolvers, caching, and query behavior.
///
/// Cache lifetimes and lookup strategy shape federation resolution.
/// Alternative resolution settings serve internal networks and application
/// services.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.network.dns",
	group = "true"
)]
pub struct Dns {
	/// Selects custom DNS servers instead of the operating system resolvers.
	///
	/// A non-empty list prevents reading `/etc/resolv.conf`. Each entry is an
	/// IP address with an optional port, defaulting to 53. Servers are
	/// expected to support both UDP and TCP on that port; enable
	/// `network.dns.query_over_tcp_only` if any server supports only TCP.
	///
	/// example: ["127.0.0.53", "1.1.1.1:5353", "[fd00::1]:53"]
	///
	/// default: []
	/// legacy: dns_servers
	#[serde(default)]
	pub servers: Vec<String>,

	/// Sets the maximum DNS memory cache entries; increase cautiously because
	/// entry sizes vary, and decrease only with an external DNS cache, excluding
	/// systemd-resolved even when configured to cache.
	///
	/// default: 32768
	/// legacy: dns_cache_entries
	#[serde(default = "default_cache_entries")]
	pub cache_entries: u32,

	/// Sets the minimum DNS cache lifetime in seconds, intentionally high by
	/// default for federation with many servers whose domains have shorter TTLs;
	/// an external cache merely shifts this load to a cache unaware of the
	/// application's needs, where honoring low TTLs can increase load.
	///
	/// default: 10800
	/// legacy: dns_min_ttl
	#[serde(default = "default_min_ttl")]
	pub min_ttl: u64,

	/// Sets the minimum cached NXDOMAIN lifetime in seconds, defaulting to three
	/// days because false negatives are very rare and efficient federation
	/// requires aggressively caching domains assumed not to return rather than
	/// repeatedly checking them.
	///
	/// default: 259200
	/// legacy: dns_min_ttl_nxdomain
	#[serde(default = "default_min_ttl_nxdomain")]
	pub min_ttl_nxdomain: u64,

	/// Sets the number of DNS nameserver retries after a timeout or error.
	///
	/// default: 10
	/// legacy: dns_attempts
	#[serde(default = "default_attempts")]
	pub attempts: u16,

	/// Sets the DNS response timeout in seconds; recursive queries to some
	/// domains can take several seconds, so avoid overly short timeouts,
	/// especially with slower hardware or resolvers.
	///
	/// default: 10
	/// legacy: dns_timeout
	#[serde(default = "default_timeout")]
	pub timeout: u64,

	/// Enables TCP fallback after DNS errors.
	///
	/// Disable this if the nameserver does not support it.
	///
	/// legacy: dns_tcp_fallback
	#[serde(default = "true_fn")]
	pub tcp_fallback: bool,

	/// Enables querying all nameservers until the domain is found.
	///
	/// Hickory resolver calls this "trust_negative_responses". It can avoid
	/// unnecessary queries when the first nameserver returns NXDOMAIN or an
	/// empty NOERROR response.
	///
	/// legacy: query_all_nameservers
	#[serde(default = "true_fn")]
	pub query_all_nameservers: bool,

	/// Queries configured nameservers using only TCP instead of UDP, as some
	/// container deployments require (see [DNS
	/// troubleshooting](https://tuwunel.chat/troubleshooting.html#potential-dns-issues-when-using-docker)).
	///
	/// legacy: query_over_tcp_only
	#[serde(default)]
	pub query_over_tcp_only: bool,

	/// Selects the DNS A/AAAA lookup strategy by number.
	///
	/// Use 1 (Ipv4Only) to query only A records with no AAAA/IPv6, or 2
	/// (Ipv6Only) to query only AAAA records with no A/IPv4. Use 3
	/// (Ipv4AndIpv6) to query both in parallel and accept the first successful
	/// response. Use 4 (Ipv6thenIpv4) to query AAAA then A on failure, or 5
	/// (Ipv4thenIpv6) to query A then AAAA on failure. Without IPv6
	/// networking, 1 can improve DNS performance because successful AAAA
	/// results cannot be used.
	///
	/// default: 5
	/// legacy: ip_lookup_strategy
	#[serde(default = "default_ip_lookup_strategy")]
	pub ip_lookup_strategy: u8,

	/// Selects domain patterns resolved through the alternative DNS path.
	///
	/// This path has no persistent cache, a very small memory cache, and no
	/// enforced TTL. It serves internal networks and application services that
	/// require those properties, and does not support federation or general
	/// use.
	///
	/// reloadable: yes
	/// example: ["*\.dns\.podman$"]
	///
	/// default: []
	/// legacy: dns_passthru_domains
	#[serde(default, with = "serde_regex")]
	pub passthru_domains: RegexSet,

	/// Resolves application services through the alternative DNS path; when all
	/// should match, prefer this to listing domains in
	/// `network.dns.passthru_domains` to avoid regex matching overhead and
	/// domain list maintenance.
	///
	/// legacy: dns_passthru_appservices
	#[serde(default)]
	pub passthru_appservices: bool,

	/// Randomizes DNS query case and requires matching answer case to mitigate
	/// spoofing; occasional logged errors usually indicate DNS overload, it can
	/// be disabled for servers lacking support, and the false default follows
	/// reports about popular DNS caches whose possible fixes may allow a future
	/// release to restore true.
	///
	/// legacy: dns_case_randomization
	#[serde(default)]
	pub case_randomization: bool,
}

/// Configures outbound HTTP clients.
///
/// Response limits and address filtering constrain remote requests.
/// Timeouts, connection reuse, compression, and certificate checks supply
/// client defaults.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.network.http",
	group = "true",
	hidden = "allow_invalid_tls_certificates"
)]
pub struct Http {
	/// Limits the size of a response body buffered from a remote server.
	///
	/// This applies to federation requests, push gateway and appservice
	/// transactions, and remote media fetched for URL previews. Peers cannot
	/// be trusted to honor requested limits, so the local bound protects
	/// against a remote exhausting process memory. Accepts an integer byte
	/// count or a string with an SI/IEC suffix such as "256 MiB".
	///
	/// default: "256 MiB"
	/// legacy: max_response_size
	#[serde(
		default = "default_max_response_size",
		deserialize_with = "deserialize_bytesize_usize"
	)]
	pub max_response_size: usize,

	/// Sets the default outbound HTTP connection timeout in seconds;
	/// individual clients may override it.
	///
	/// default: 10
	/// legacy: request_conn_timeout
	#[serde(default = "default_conn_timeout")]
	pub conn_timeout: u64,

	/// Sets the default outbound HTTP read timeout in seconds; individual
	/// clients may override it.
	///
	/// default: 35
	/// legacy: request_timeout
	#[serde(default = "default_request_timeout")]
	pub timeout: u64,

	/// Sets the default outbound HTTP whole-request timeout in seconds.
	///
	/// Keep it high enough for healthy requests while retaining a timeout
	/// backstop. Individual clients may override it.
	///
	/// default: 320
	/// legacy: request_total_timeout
	#[serde(default = "default_total_timeout")]
	pub total_timeout: u64,

	/// Sets the default outbound HTTP idle connection pool timeout in seconds;
	/// individual clients may override it.
	///
	/// default: 5
	/// legacy: request_idle_timeout
	#[serde(default = "default_idle_timeout")]
	pub idle_timeout: u64,

	/// Sets the default maximum idle outbound HTTP connections per host;
	/// individual clients may override it.
	///
	/// default: 1
	/// legacy: request_idle_per_host
	#[serde(default = "default_idle_per_host")]
	pub idle_per_host: u16,

	/// Allows outbound HTTP clients to negotiate gzip response compression.
	///
	/// Clients advertise gzip in `Accept-Encoding` and transparently
	/// decompress responses for federation, media, and URL preview traffic.
	/// This is separate from `http.compression.gzip`, which compresses
	/// tuwunel's own responses. It is enabled by default; disabling it
	/// prevents both requesting and decompressing gzip. It has no effect
	/// unless tuwunel was built with the `gzip_compression` feature.
	///
	/// default: true
	/// legacy: request_gzip
	#[serde(default = "true_fn")]
	pub gzip: bool,

	/// Allows outbound HTTP clients to negotiate brotli response compression.
	///
	/// Clients advertise brotli in `Accept-Encoding` and transparently
	/// decompress responses for federation, media, and URL preview traffic.
	/// This is separate from `http.compression.brotli`, which compresses
	/// tuwunel's own responses. It is enabled by default; disabling it
	/// prevents both requesting and decompressing brotli. It has no effect
	/// unless tuwunel was built with the `brotli_compression` feature.
	///
	/// default: true
	/// legacy: request_brotli
	#[serde(default = "true_fn")]
	pub brotli: bool,

	/// Allows outbound HTTP clients to negotiate zstd response compression.
	///
	/// Clients advertise zstd in `Accept-Encoding` and transparently
	/// decompress responses for federation, media, and URL preview traffic.
	/// This is separate from `http.compression.zstd`, which compresses
	/// tuwunel's own responses. It is enabled by default; disabling it
	/// prevents both requesting and decompressing zstd. It has no effect
	/// unless tuwunel was built with the `zstd_compression` feature.
	///
	/// default: true
	/// legacy: request_zstd
	#[serde(default = "true_fn")]
	pub zstd: bool,

	#[expect(clippy::doc_link_with_quotes)]
	/// Blocks outbound requests to the listed IPv4 and IPv6 CIDR ranges.
	///
	/// Entries are quoted strings; the defaults cover RFC1918, unroutable,
	/// loopback, multicast, and testnet addresses for security, and an empty
	/// vector (`[]`) disables this filter. Application filtering can have
	/// bypasses, so enforce network boundaries with firewall zones rather than
	/// treating this as a guarantee.
	///
	/// Proxy endpoints selected through configuration or environment variables
	/// are exempt so private forward proxies remain reachable, while direct
	/// requests and locally resolving `socks4` or `socks5` proxies still
	/// filter destination addresses. HTTP(S) forward proxies and `socks4a` or
	/// `socks5h` resolve destinations remotely, so their egress policy must
	/// enforce destination network boundaries instead.
	///
	/// Defaults to:
	/// ["127.0.0.0/8", "0.0.0.0/8", "10.0.0.0/8", "172.16.0.0/12",
	/// "192.168.0.0/16", "100.64.0.0/10", "192.0.0.0/24", "169.254.0.0/16",
	/// "192.88.99.0/24", "198.18.0.0/15", "192.0.2.0/24", "198.51.100.0/24",
	/// "203.0.113.0/24", "224.0.0.0/4", "::1/128", "::/128", "fe80::/10",
	/// "fc00::/7", "2001:db8::/32", "ff00::/8", "fec0::/10"]
	/// legacy: ip_range_denylist
	#[serde(default = "default_ip_range_denylist")]
	pub ip_range_denylist: Vec<String>,

	/// Disables TLS certificate validation.
	///
	/// This hidden setting affects all requests, including URL previews and
	/// federation. It is highly insecure and must not be used in production.
	///
	/// legacy: allow_invalid_tls_certificates
	#[serde(default)]
	pub allow_invalid_tls_certificates: bool,
}

impl Default for Discovery {
	fn default() -> Self {
		Self {
			conn_timeout: default_well_known_conn_timeout(),
			timeout: default_well_known_timeout(),
		}
	}
}

impl Default for Dns {
	fn default() -> Self {
		Self {
			servers: Default::default(),
			cache_entries: default_cache_entries(),
			min_ttl: default_min_ttl(),
			min_ttl_nxdomain: default_min_ttl_nxdomain(),
			attempts: default_attempts(),
			timeout: default_timeout(),
			tcp_fallback: true_fn(),
			query_all_nameservers: true_fn(),
			query_over_tcp_only: Default::default(),
			ip_lookup_strategy: default_ip_lookup_strategy(),
			passthru_domains: Default::default(),
			passthru_appservices: Default::default(),
			case_randomization: Default::default(),
		}
	}
}

impl Default for Http {
	fn default() -> Self {
		Self {
			max_response_size: default_max_response_size(),
			conn_timeout: default_conn_timeout(),
			timeout: default_request_timeout(),
			total_timeout: default_total_timeout(),
			idle_timeout: default_idle_timeout(),
			idle_per_host: default_idle_per_host(),
			gzip: true_fn(),
			brotli: true_fn(),
			zstd: true_fn(),
			ip_range_denylist: default_ip_range_denylist(),
			allow_invalid_tls_certificates: Default::default(),
		}
	}
}

fn default_well_known_conn_timeout() -> u64 { 6 }

fn default_well_known_timeout() -> u64 { 10 }

fn default_cache_entries() -> u32 { 32768 }

fn default_min_ttl() -> u64 { 60 * 180 }

fn default_min_ttl_nxdomain() -> u64 { 60 * 60 * 24 * 3 }

fn default_attempts() -> u16 { 10 }

fn default_timeout() -> u64 { 10 }

fn default_ip_lookup_strategy() -> u8 { 5 }

fn default_max_response_size() -> usize { 256 * 1024 * 1024 }

fn default_conn_timeout() -> u64 { 10 }

fn default_request_timeout() -> u64 { 35 }

fn default_total_timeout() -> u64 { 320 }

fn default_idle_timeout() -> u64 { 5 }

fn default_idle_per_host() -> u16 { 1 }

fn default_ip_range_denylist() -> Vec<String> {
	vec![
		"127.0.0.0/8".to_owned(),
		"0.0.0.0/8".to_owned(),
		"10.0.0.0/8".to_owned(),
		"172.16.0.0/12".to_owned(),
		"192.168.0.0/16".to_owned(),
		"100.64.0.0/10".to_owned(),
		"192.0.0.0/24".to_owned(),
		"169.254.0.0/16".to_owned(),
		"192.88.99.0/24".to_owned(),
		"198.18.0.0/15".to_owned(),
		"192.0.2.0/24".to_owned(),
		"198.51.100.0/24".to_owned(),
		"203.0.113.0/24".to_owned(),
		"224.0.0.0/4".to_owned(),
		"::1/128".to_owned(),
		"::/128".to_owned(),
		"fe80::/10".to_owned(),
		"fc00::/7".to_owned(),
		"2001:db8::/32".to_owned(),
		"ff00::/8".to_owned(),
		"fec0::/10".to_owned(),
	]
}
