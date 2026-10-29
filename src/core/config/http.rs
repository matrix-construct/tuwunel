//! Configures incoming HTTP requests, listeners, and response compression.
//!
//! Listener settings select TCP addresses, ports, and UNIX sockets. Request
//! limits, proxy address sources, and compression apply to inbound traffic.

use std::{collections::BTreeSet, path::PathBuf};

use either::Either::Left;
use ipnet::IpNet;
use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::{
	IpSource, deserialize_bytesize_usize,
	net::{ListeningAddr, ListeningPort},
};

/// Configures inbound HTTP requests and responses.
///
/// Request limits and client address extraction apply to server traffic.
/// Listener and compression settings control how that traffic reaches
/// clients.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.http",
	group = "true",
	ignore = "compression listen"
)]
pub struct Http {
	/// Sets the maximum request size for file uploads.
	///
	/// Accepts an integer byte count or a string with an SI/IEC suffix such as
	/// "24 MiB".
	///
	/// default: "24 MiB"
	/// legacy: max_request_size
	#[serde(
		default = "default_max_request_size",
		deserialize_with = "deserialize_bytesize_usize"
	)]
	pub max_request_size: usize,

	/// Sets the maximum time in seconds to receive a client request.
	///
	/// default: 75
	/// legacy: client_receive_timeout
	#[serde(default = "default_receive_timeout")]
	pub receive_timeout: u64,

	/// Sets the maximum time in seconds to process a client request.
	///
	/// default: 240
	/// legacy: client_request_timeout
	#[serde(default = "default_request_timeout")]
	pub request_timeout: u64,

	/// Sets the maximum time in seconds to transmit a client response.
	///
	/// default: 120
	/// legacy: client_response_timeout
	#[serde(default = "default_response_timeout")]
	pub response_timeout: u64,

	/// Sets the grace period in seconds for clean shutdown of client requests.
	///
	/// reloadable: yes
	/// default: 15
	/// legacy: client_shutdown_timeout
	#[serde(default = "default_shutdown_timeout")]
	pub shutdown_timeout: u64,

	/// Selects the client IP source for rate limiting, logging, and security
	/// tooling.
	///
	/// When unset, `ClientIp` scans common proxy headers in leftmost-IP mode
	/// (`X-Forwarded-For`, RFC 7239 `Forwarded`, `X-Real-IP`, `Fly-Client-IP`,
	/// `True-Client-IP`, `CF-Connecting-IP`, `CloudFront-Viewer-Address`)
	/// before falling back to the TCP peer address, allowing clients to spoof
	/// their address through request headers. When set, extraction uses only
	/// the selected source and takes the rightmost value of multi-valued
	/// headers, where only the proxy can append, resisting client spoofing;
	/// header sources still require a trusted reverse proxy in front of
	/// tuwunel to prevent forgery.
	///
	/// Supported values are "connect_info" for direct TCP peers,
	/// "rightmost_x_forwarded_for" for nginx or Caddy, "rightmost_forwarded"
	/// for RFC 7239 proxies, "x_real_ip" for nginx `X-Real-IP`,
	/// "cf_connecting_ip" for Cloudflare or cloudflared, "true_client_ip" for
	/// Akamai or Cloudflare Enterprise, "fly_client_ip" for Fly.io, and
	/// "cloudfront_viewer_address" for AWS CloudFront. Unix-socket deployments
	/// should leave this unset instead of using "connect_info", which requires
	/// a TCP peer address; changing this setting requires a server restart.
	///
	/// default: unset
	/// config-example: "connect_info"
	/// legacy: ip_source
	#[serde(default)]
	pub ip_source: Option<IpSource>,

	/// Selects TCP peer subnets that bypass the configured client IP source.
	///
	/// Matching peers use the insecure header scan and `ConnectInfo` fallback
	/// used when `http.ip_source` is unset; entries require CIDR prefixes
	/// (`/32` or `/128` for a single host), while loopback (`127.0.0.0/8`,
	/// `::1/128`) always bypasses extraction without being listed. Use this
	/// for locally attached bridges or server-side clients on private
	/// container or VPN subnets that cannot carry the configured proxy header,
	/// such as a Docker bridge without `network_mode: host`.
	///
	/// Exclude the address receiving external traffic from your proxy: listing
	/// a shared bridge subnet containing the proxy, such as a common Compose
	/// network, also bypasses proxy traffic and defeats `http.ip_source`. Any
	/// peer in these subnets can forge client IPs through request headers, so
	/// include only subnets controlled end-to-end; changing this setting
	/// requires a server restart.
	///
	/// default: []
	/// config-example: ["172.18.0.0/16", "fd00::/8"]
	/// legacy: ip_source_trusted_subnets
	#[expect(
		clippy::doc_link_with_quotes,
		reason = "config-example directive emits literal quoted strings, not an intra-doc link"
	)]
	#[serde(default)]
	pub ip_source_trusted_subnets: Vec<IpNet>,

	/// Sets the `Access-Control-Allow-Origin` header included in all
	/// responses.
	///
	/// Multiple values may be specified. The default empty list produces the
	/// header value `*`. Configure this only for a specific purpose, because
	/// incorrect values can degrade or disrupt clients.
	///
	/// default: []
	/// legacy: access_control_allow_origin
	#[serde(default)]
	pub access_control_allow_origin: BTreeSet<String>,

	/// Settings for http compression.
	///
	/// display: section
	#[serde(default)]
	pub compression: Compression,

	/// Settings for http listen.
	///
	/// display: section
	#[serde(default)]
	pub listen: Listen,
}

/// Selects compression for outbound server responses.
///
/// Each codec requires its matching build feature. These settings affect
/// server responses independently of outbound HTTP client negotiation.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.http.compression",
	group = "true"
)]
pub struct Compression {
	/// Compresses the server's HTTP response bodies with zstd.
	///
	/// This has no effect unless tuwunel was built with the `zstd_compression`
	/// feature. Most users do not need it, and HTTP compression can weaken
	/// TLS; review https://breachattack.com/ and
	/// https://wikipedia.org/wiki/BREACH before enabling it.
	///
	/// legacy: zstd_compression
	#[serde(default)]
	pub zstd: bool,

	/// Compresses the server's HTTP response bodies with gzip.
	///
	/// This has no effect unless tuwunel was built with the `gzip_compression`
	/// feature. Most users do not need it, and HTTP compression can weaken
	/// TLS; review https://breachattack.com/ and
	/// https://wikipedia.org/wiki/BREACH before enabling it. Servers in many
	/// rooms may need it to reduce very large response bodies.
	///
	/// legacy: gzip_compression
	#[serde(default)]
	pub gzip: bool,

	/// Compresses the server's HTTP response bodies with brotli.
	///
	/// This has no effect unless tuwunel was built with the
	/// `brotli_compression` feature. Most users do not need it, and HTTP
	/// compression can weaken TLS; review https://breachattack.com/ and
	/// https://wikipedia.org/wiki/BREACH before enabling it.
	///
	/// legacy: brotli_compression
	#[serde(default)]
	pub brotli: bool,
}

/// Configures TCP and UNIX socket listeners.
///
/// Explicit TCP addresses must bind successfully during startup. UNIX
/// socket settings control the path and permissions used by reverse
/// proxies.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.http.listen",
	group = "true"
)]
pub struct Listen {
	/// Selects the IPv4 or IPv6 addresses on which tuwunel listens.
	///
	/// Docker or container NAT deployments require "0.0.0.0". To listen on
	/// multiple addresses, use a vector such as ["127.0.0.1", "::1"]. Every
	/// explicitly configured address must bind or startup fails. The default
	/// only assumes both loopback families exist, so a default address that
	/// cannot bind is logged and skipped.
	///
	/// default: ["127.0.0.1", "::1"]
	/// legacy: address
	#[allow(
		clippy::doc_link_with_quotes,
		clippy::allow_attributes,
		reason = "configuration arrays contain literal quoted strings, not intra-doc links"
	)]
	#[serde(default)]
	pub(super) address: Option<ListeningAddr>,

	/// Selects the ports on which tuwunel listens.
	///
	/// For reverse proxy setup, see
	/// https://tuwunel.chat/deploying/generic.html#setting-up-the-reverse-proxy.
	/// Docker deployments should keep this value and map an external port to
	/// it. To listen on multiple ports, use a vector such as [8080, 8448].
	///
	/// default: 8008
	/// legacy: port
	#[serde(default = "default_port")]
	pub(super) port: ListeningPort,

	/// Selects the UNIX socket on which tuwunel listens.
	///
	/// The reverse proxy must be able to access this socket file. Add the
	/// proxy to the 'tuwunel' group or grant world read/write access with
	/// `http.listen.unix_socket_perms` set to at least 666.
	///
	/// example: "/run/tuwunel/tuwunel.sock"
	/// legacy: unix_socket_path
	pub unix_socket_path: Option<PathBuf>,

	/// Sets the default permissions, in octal, for creating the UNIX socket.
	///
	/// default: 660
	/// legacy: unix_socket_perms
	#[serde(default = "default_unix_socket_perms")]
	pub unix_socket_perms: u32,
}

impl Default for Http {
	fn default() -> Self {
		Self {
			max_request_size: default_max_request_size(),
			receive_timeout: default_receive_timeout(),
			request_timeout: default_request_timeout(),
			response_timeout: default_response_timeout(),
			shutdown_timeout: default_shutdown_timeout(),
			ip_source: Default::default(),
			ip_source_trusted_subnets: Default::default(),
			access_control_allow_origin: Default::default(),
			compression: Compression::default(),
			listen: Listen::default(),
		}
	}
}

impl Default for Listen {
	fn default() -> Self {
		Self {
			address: Default::default(),
			port: default_port(),
			unix_socket_path: None,
			unix_socket_perms: default_unix_socket_perms(),
		}
	}
}

fn default_max_request_size() -> usize { 24 * 1024 * 1024 }

fn default_receive_timeout() -> u64 { 75 }

fn default_request_timeout() -> u64 { 240 }

fn default_response_timeout() -> u64 { 120 }

fn default_shutdown_timeout() -> u64 { 15 }

fn default_port() -> ListeningPort { ListeningPort { ports: Left(8008) } }

fn default_unix_socket_perms() -> u32 { 660 }
