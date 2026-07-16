use std::{
	net::IpAddr,
	ops::Deref,
	sync::{Arc, LazyLock},
	time::Duration,
};

use bytes::{Bytes, BytesMut};
use ipaddress::{IPAddress, ipv4::from_u32 as ipv4_from_u32};
use reqwest::{Certificate, Client, ClientBuilder, dns::Resolve, header::HeaderValue, redirect};
use tuwunel_core::{Config, Err, Result, debug, either::Either, err, implement, trace};

use crate::{Services, resolver::Validating, service};

type DisableEncoding = fn(ClientBuilder) -> ClientBuilder;

pub struct Clients {
	pub default: Client,
	pub url_preview: Client,
	pub url_preview_media: Client,
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

	pub cidr_range_denylist: Arc<[IPAddress]>,
}

impl Deref for Service {
	type Target = Clients;

	fn deref(&self) -> &Self::Target { &self.clients }
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;

		Ok(Arc::new(Self {
			clients: LazyLock::new(Box::new({
				let services = args.services.clone();

				move || make_clients(&services).expect("failed to construct clients")
			})),

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

fn make_clients(services: &Services) -> Result<Clients> {
	macro_rules! with {
		($builder:ident => $make:expr) => {{
			let $builder = base(&services.config, None)?;
			$make.build()?
		}};
		($name:literal, $builder:ident => $make:expr) => {{
			let $builder = base(&services.config, Some($name))?;
			$make.build()?
		}};
	}

	Ok(Clients {
		default: with!(cb => cb.dns_resolver(Arc::clone(&services.resolver.resolver))),

		url_preview: with!("preview", cb => preview_builder(
			services,
			cb,
			services
				.config
				.url_preview_user_agent
				.as_deref(),
		)?),

		url_preview_media: with!("preview", cb => preview_builder(
			services,
			cb,
			services
				.config
				.url_preview_media_user_agent
				.as_deref()
				.or(services
					.config
					.url_preview_user_agent
					.as_deref()),
		)?),

		extern_media: with!(cb => cb
			.dns_resolver(Validating::new(
				Arc::clone(&services.resolver.resolver),
				Arc::clone(&services.client.cidr_range_denylist),
			))
			.redirect(redirect::Policy::limited(3))),

		well_known: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver))
			.connect_timeout(Duration::from_secs(
				services.config.well_known_conn_timeout,
			))
			.read_timeout(Duration::from_secs(services.config.well_known_timeout))
			.timeout(Duration::from_secs(services.config.well_known_timeout))
			.pool_max_idle_per_host(0)
			.redirect(redirect::Policy::limited(4))),

		federation: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver.hooked))
			.read_timeout(Duration::from_secs(services.config.federation_timeout))
			.pool_max_idle_per_host(services.config.federation_idle_per_host.into())
			.pool_idle_timeout(Duration::from_secs(
				services.config.federation_idle_timeout,
			))
			.redirect(redirect::Policy::limited(3))),

		synapse: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver.hooked))
			.read_timeout(Duration::from_secs(305))
			.pool_max_idle_per_host(0)
			.redirect(redirect::Policy::limited(3))),

		sender: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver.hooked))
			.read_timeout(Duration::from_secs(services.config.sender_timeout))
			.timeout(Duration::from_secs(services.config.sender_timeout))
			.pool_max_idle_per_host(1)
			.pool_idle_timeout(Duration::from_secs(
				services.config.sender_idle_timeout,
			))
			.redirect(redirect::Policy::limited(2))),

		appservice: with!(cb => cb
			.dns_resolver(appservice_resolver(services))
			.connect_timeout(Duration::from_secs(5))
			.read_timeout(Duration::from_secs(services.config.appservice_timeout))
			.timeout(Duration::from_secs(services.config.appservice_timeout))
			.pool_max_idle_per_host(1)
			.pool_idle_timeout(Duration::from_secs(
				services.config.appservice_idle_timeout,
			))
			.redirect(redirect::Policy::limited(2))),

		pusher: with!(cb => cb
			.dns_resolver(Validating::new(
				Arc::clone(&services.resolver.resolver),
				Arc::clone(&services.client.cidr_range_denylist),
			))
			.pool_max_idle_per_host(1)
			.pool_idle_timeout(Duration::from_secs(
				services.config.pusher_idle_timeout,
			))
			.redirect(redirect::Policy::limited(2))),

		oauth: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver))
			.redirect(redirect::Policy::limited(0))
			.pool_max_idle_per_host(1)),
	})
}

/// Shared construction for the URL preview clients: bound to the configured
/// interface, resolving through the CIDR-validating resolver, with an
/// optional User-Agent override.
fn preview_builder(
	services: &Services,
	builder: ClientBuilder,
	user_agent: Option<&str>,
) -> Result<ClientBuilder> {
	let interface = &services.config.url_preview_bound_interface;

	let bind_addr = interface.clone().and_then(Either::left);
	let bind_iface = interface.clone().and_then(Either::right);

	let resolver = Validating::new(
		Arc::clone(&services.resolver.resolver),
		Arc::clone(&services.client.cidr_range_denylist),
	);

	let builder = match user_agent {
		| Some(user_agent) => builder.user_agent(user_agent),
		| None => builder,
	};

	Ok(builder_interface(builder, bind_iface.as_deref())?
		.local_address(bind_addr)
		.dns_resolver(resolver)
		.redirect(redirect::Policy::limited(3)))
}

fn base(config: &Config, name: Option<&str>) -> Result<ClientBuilder> {
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
		.redirect(redirect::Policy::limited(6))
		.danger_accept_invalid_certs(config.allow_invalid_tls_certificates)
		.tls_certs_merge(
			webpki_root_certs::TLS_SERVER_ROOT_CERTS
				.iter()
				.map(|der| {
					Certificate::from_der(der).expect("certificate must be valid der encoding")
				}),
		)
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

	match config.proxy.to_proxy()? {
		| Some(proxy) => Ok(builder.proxy(proxy)),
		| _ => Ok(builder),
	}
}

/// Buffer a remote response body, rejecting any response larger than `limit`
/// bytes. reqwest enforces no response-size limit, so an unbounded `bytes()`
/// lets a peer drive an allocator abort; refuse an oversized advertised length
/// and hold the same bound while streaming for when that length is absent.
pub async fn read_response_capped(
	mut response: reqwest::Response,
	limit: usize,
) -> Result<Bytes> {
	let mut body = match response.content_length() {
		| Some(len) if len > limit.try_into().unwrap_or(u64::MAX) => {
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
