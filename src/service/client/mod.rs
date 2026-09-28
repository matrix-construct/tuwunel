use std::{
	net::{IpAddr, SocketAddr},
	ops::Deref,
	sync::{Arc, LazyLock},
	time::Duration,
};

use bytes::{Bytes, BytesMut};
use ipaddress::{IPAddress, ipv4::from_u32 as ipv4_from_u32};
use reqwest::{Client, ClientBuilder, Url, dns::Resolve, header::HeaderValue, redirect::Policy};
use tuwunel_core::{
	Config, Err, Result, config::proxy::ProxySnapshot, debug, either::Either, err,
	error::error_chain, implement, trace, utils::math::u64_from_usize_saturating,
};
use url::Host;

use crate::{Services, resolver::Validating, service};

#[cfg(test)]
mod tests;

type DisableEncoding = fn(ClientBuilder) -> ClientBuilder;

pub struct Clients {
	pub default: Client,
	pub url_preview: Client,
	pub extern_media: Client,
	pub well_known: Client,
	pub federation: Client,
	pub synapse: Client,
	pub sender: Client,
	pub appservice: Client,
	pub pusher: Client,
	pub oauth: Client,
}

pub struct Service {
	pub clients: LazyLock<Clients, Box<dyn FnOnce() -> Clients + Send>>,

	/// Effective proxy policy for this generation of clients.
	///
	/// The snapshot keeps client routing, DNS exemptions, and response checks
	/// aligned even if configuration or environment values later change.
	pub proxy: Arc<ProxySnapshot>,
	pub cidr_range_denylist: Arc<[IPAddress]>,
}

impl Deref for Service {
	type Target = Clients;

	fn deref(&self) -> &Self::Target { &self.clients }
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;
		let proxy = Arc::new(ProxySnapshot::new(&config.proxy)?);

		probe_tls(config, &proxy)?;

		Ok(Arc::new(Self {
			clients: LazyLock::new(Box::new({
				let services = args.services.clone();

				move || make_clients(&services).expect("failed to construct clients")
			})),

			proxy,
			cidr_range_denylist: config
				.ip_range_denylist
				.iter()
				.map(IPAddress::parse)
				.inspect(|cidr| trace!("Denied CIDR range: {cidr:?}"))
				.collect::<Result<Vec<_>, String>>()
				.map(Arc::from)
				.map_err(|e| err!(Config("ip_range_denylist", e)))?,
		}))
	}

	fn name(&self) -> &str { service::make_name(std::module_path!()) }
}

/// Fails startup when an HTTPS client cannot be constructed.
///
/// The clients are built lazily on first use, so a platform trust store that
/// yields no roots surfaces as a panic inside whichever worker reaches for a
/// client first. Probing once at startup turns that into a legible boot error.
fn probe_tls(config: &Config, proxy: &ProxySnapshot) -> Result {
	base(config, proxy, None)?
		.build()
		.map(drop)
		.map_err(|e| {
			err!(error!(
				chain = %error_chain(&e),
				"Failed to construct an HTTPS client. If the system trust store is empty, \
				 install a CA bundle (ca-certificates on Debian and Ubuntu) or point \
				 SSL_CERT_FILE at one.",
			))
		})
}

fn make_clients(services: &Services) -> Result<Clients> {
	macro_rules! with {
		($builder:ident => $make:expr) => {{
			let $builder = base(&services.config, &services.client.proxy, None)?;

			$make.build()?
		}};
		($name:literal, $builder:ident => $make:expr) => {{
			let $builder = base(&services.config, &services.client.proxy, Some($name))?;

			$make.build()?
		}};
	}

	Ok(Clients {
		default: with!(cb => cb.dns_resolver(Arc::clone(&services.resolver.resolver))),

		url_preview: with!("preview", cb => preview_builder(services, cb)?),

		extern_media: with!(cb => cb
			.dns_resolver(Validating::new(
				Arc::clone(&services.resolver.resolver),
				Arc::clone(&services.client.cidr_range_denylist),
				services.client.proxy.shared_hosts(),
			))
			.redirect(guarded_redirect(services, 3))),

		well_known: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver))
			.connect_timeout(Duration::from_secs(
				services.config.well_known_conn_timeout,
			))
			.read_timeout(Duration::from_secs(services.config.well_known_timeout))
			.timeout(Duration::from_secs(services.config.well_known_timeout))
			.pool_max_idle_per_host(0)
			.redirect(Policy::limited(4))),

		federation: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver.hooked))
			.read_timeout(Duration::from_secs(services.config.federation_timeout))
			.pool_max_idle_per_host(services.config.federation_idle_per_host.into())
			.pool_idle_timeout(Duration::from_secs(
				services.config.federation_idle_timeout,
			))
			.redirect(Policy::limited(3))),

		synapse: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver.hooked))
			.read_timeout(Duration::from_secs(305))
			.pool_max_idle_per_host(0)
			.redirect(Policy::limited(3))),

		sender: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver.hooked))
			.read_timeout(Duration::from_secs(services.config.sender_timeout))
			.timeout(Duration::from_secs(services.config.sender_timeout))
			.pool_max_idle_per_host(1)
			.pool_idle_timeout(Duration::from_secs(
				services.config.sender_idle_timeout,
			))
			.redirect(Policy::limited(2))),

		appservice: with!(cb => cb
			.dns_resolver(appservice_resolver(services))
			.connect_timeout(Duration::from_secs(5))
			.read_timeout(Duration::from_secs(services.config.appservice_timeout))
			.timeout(Duration::from_secs(services.config.appservice_timeout))
			.pool_max_idle_per_host(1)
			.pool_idle_timeout(Duration::from_secs(
				services.config.appservice_idle_timeout,
			))
			.redirect(Policy::limited(2))),

		pusher: with!(cb => cb
			.dns_resolver(Validating::new(
				Arc::clone(&services.resolver.resolver),
				Arc::clone(&services.client.cidr_range_denylist),
				services.client.proxy.shared_hosts(),
			))
			.pool_max_idle_per_host(1)
			.pool_idle_timeout(Duration::from_secs(
				services.config.pusher_idle_timeout,
			))
			.redirect(guarded_redirect(services, 2))),

		oauth: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver))
			.redirect(Policy::limited(0))
			.pool_max_idle_per_host(1)),
	})
}

/// Construction for the URL preview client: bound to the configured
/// interface and resolving through the CIDR-validating resolver.
///
/// The configured User-Agent is applied per request rather than here, so a
/// configuration reload takes effect without rebuilding the client.
fn preview_builder(services: &Services, builder: ClientBuilder) -> Result<ClientBuilder> {
	let interface = &services.config.url_preview_bound_interface;

	let bind_addr = interface.clone().and_then(Either::left);
	let bind_iface = interface.clone().and_then(Either::right);

	let resolver = Validating::new(
		Arc::clone(&services.resolver.resolver),
		Arc::clone(&services.client.cidr_range_denylist),
		services.client.proxy.shared_hosts(),
	);

	Ok(builder_interface(builder, bind_iface.as_deref())?
		.local_address(bind_addr)
		.dns_resolver(resolver)
		.redirect(guarded_redirect(services, 3)))
}

fn guarded_redirect(services: &Services, max: usize) -> Policy {
	let proxy = Arc::clone(&services.client.proxy);
	let denylist = Arc::clone(&services.client.cidr_range_denylist);
	let limited = Policy::limited(max);

	Policy::custom(move |attempt| {
		if proxy.resolver_alias(attempt.url()) || !valid_cidr_range_url(&denylist, attempt.url())
		{
			attempt.error("redirect destination is not allowed")
		} else {
			limited.redirect(attempt)
		}
	})
}

fn valid_cidr_range_url(denylist: &[IPAddress], url: &Url) -> bool {
	let ip = match url.host() {
		| Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
		| Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
		| Some(Host::Domain(_)) | None => return true,
	};

	let ip = ipaddress_from_std(ip);

	denylist.iter().all(|cidr| !cidr.includes(&ip))
}

fn base(config: &Config, proxy: &ProxySnapshot, name: Option<&str>) -> Result<ClientBuilder> {
	let user_agent = tuwunel_core::version::user_agent();
	let user_agent: HeaderValue = name
		.map(|name| format!("{user_agent} {name}").try_into())
		.unwrap_or_else(|| user_agent.try_into())?;

	let builder = Client::builder()
		.connect_timeout(Duration::from_secs(config.request_conn_timeout))
		.read_timeout(Duration::from_secs(config.request_timeout))
		.timeout(Duration::from_secs(config.request_total_timeout))
		.pool_idle_timeout(Duration::from_secs(config.request_idle_timeout))
		.pool_max_idle_per_host(config.request_idle_per_host.into())
		.user_agent(user_agent)
		.redirect(Policy::limited(6))
		.danger_accept_invalid_certs(config.allow_invalid_tls_certificates)
		.connection_verbose(cfg!(debug_assertions))
		// Check if env var is set to avoid locking the keyfile mutex on every connection open
		.tls_sslkeylogfile(std::env::var_os("SSLKEYLOGFILE").is_some());

	let encodings: [(bool, DisableEncoding); 3] = [
		(config.request_gzip, ClientBuilder::no_gzip),
		(config.request_brotli, ClientBuilder::no_brotli),
		(config.request_zstd, ClientBuilder::no_zstd),
	];

	let builder = encodings
		.into_iter()
		.filter(|(enabled, _)| !enabled)
		.fold(builder, |builder, (_, disable)| disable(builder));

	Ok(proxy.configure(builder))
}

/// Prevents a remote peer from forcing unbounded response-body allocation.
///
/// An advertised `Content-Length` above `limit` is rejected before the body
/// buffer is allocated. The streaming loop enforces the same bound when the
/// length is absent or inaccurate.
pub async fn read_response_capped(
	mut response: reqwest::Response,
	limit: usize,
) -> Result<Bytes> {
	let mut body = match response.content_length() {
		| Some(len) if len > u64_from_usize_saturating(limit) => {
			debug!(%len, %limit, "rejecting response: advertised body exceeds limit");
			return Err!(BadServerResponse(
				"Response body length {len} exceeds the {limit} byte limit"
			));
		},
		| Some(len) => BytesMut::with_capacity(usize::try_from(len).unwrap_or(limit)),
		| None => BytesMut::new(),
	};
	while let Some(chunk) = response.chunk().await? {
		if body.len().saturating_add(chunk.len()) > limit {
			debug!(%limit, "rejecting response: streamed body exceeds limit");
			return Err!(BadServerResponse("Response body exceeds the {limit} byte limit"));
		}

		body.extend_from_slice(&chunk);
	}

	Ok(body.freeze())
}

#[cfg(any(
	target_os = "android",
	target_os = "fuchsia",
	target_os = "linux"
))]
fn builder_interface(builder: ClientBuilder, config: Option<&str>) -> Result<ClientBuilder> {
	if let Some(iface) = config {
		Ok(builder.interface(iface))
	} else {
		Ok(builder)
	}
}

#[cfg(not(any(
	target_os = "android",
	target_os = "fuchsia",
	target_os = "linux"
)))]
fn builder_interface(builder: ClientBuilder, config: Option<&str>) -> Result<ClientBuilder> {
	use tuwunel_core::Err;

	config.map_or_else(
		|| Ok(builder),
		|iface| {
			Err!(
				"Binding to network-interface {iface:?} by name is not supported on this \
				 platform."
			)
		},
	)
}

fn appservice_resolver(services: &Services) -> Arc<dyn Resolve> {
	if services.server.config.dns_passthru_appservices {
		services.resolver.resolver.passthru.clone()
	} else {
		services.resolver.resolver.clone()
	}
}

/// Checks a response peer against the CIDR policy used by guarded clients.
///
/// A proxied response is screened at the proxy route instead; direct responses
/// remain subject to the destination denylist.
#[implement(Service)]
#[inline]
#[must_use]
pub fn valid_cidr_range_remote_addr(&self, url: &Url, remote_addr: SocketAddr) -> bool {
	self.valid_cidr_range_ip(remote_addr.ip()) || self.proxied(url)
}

#[inline]
#[must_use]
#[implement(Service)]
pub fn valid_cidr_range(&self, ip: &IPAddress) -> bool {
	self.cidr_range_denylist
		.iter()
		.all(|cidr| !cidr.includes(ip))
}

#[inline]
#[must_use]
#[implement(Service)]
pub fn valid_cidr_range_ip(&self, ip: IpAddr) -> bool {
	let addr = ipaddress_from_std(ip);
	self.cidr_range_denylist
		.iter()
		.all(|cidr| !cidr.includes(&addr))
}

/// Checks an HTTP URL against the CIDR denylist when its host is an IP literal.
///
/// Domain names pass here because the validating resolver screens their
/// addresses later, immediately before connection.
#[implement(Service)]
#[inline]
#[must_use]
pub fn valid_cidr_range_url(&self, url: &Url) -> bool {
	valid_cidr_range_url(&self.cidr_range_denylist, url)
}

/// Reports whether this client generation proxies a request URL.
///
/// The result uses the same snapshotted configured or environment route that
/// was applied when the clients were built.
#[implement(Service)]
#[inline]
#[must_use]
pub fn proxied(&self, url: &Url) -> bool { self.proxy.intercepts(url) }

#[must_use]
pub(crate) fn ipaddress_from_std(ip: IpAddr) -> IPAddress {
	match ip {
		| IpAddr::V4(v4) =>
			ipv4_from_u32(u32::from(v4), 32).expect("/32 is always a valid prefix"),
		// ipv6::from_int would skip the regex parser but pulls in num-bigint.
		| IpAddr::V6(v6) =>
			IPAddress::parse(v6.to_string()).expect("Ipv6Addr Display output parses"),
	}
}
