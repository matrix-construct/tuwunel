use std::{
	sync::{Arc, LazyLock},
	time::Duration,
};

use ipaddress::IPAddress;
use reqwest::{dns::Resolve, redirect};
use tuwunel_core::{Config, Result, either::Either, err, implement, trace};

use crate::{service, services::OnceServices};

type ClientLazylock = LazyLock<reqwest::Client, Box<dyn FnOnce() -> reqwest::Client + Send>>;
pub struct Service {
	pub default: ClientLazylock,
	pub url_preview: ClientLazylock,
	pub extern_media: ClientLazylock,
	pub well_known: ClientLazylock,
	pub federation: ClientLazylock,
	pub synapse: ClientLazylock,
	pub sender: ClientLazylock,
	pub appservice: ClientLazylock,
	pub pusher: ClientLazylock,
	pub oauth: ClientLazylock,

	pub cidr_range_denylist: Vec<IPAddress>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;

		macro_rules! create_client {
			($config:ident, $services:ident; $expr:expr) => {{
				fn make($services: Arc<OnceServices>) -> Result<reqwest::Client> {
					let $config = &$services.server.config;
					Ok($expr.build()?)
				}
				let services = Arc::clone(args.services);
				LazyLock::new(Box::new(|| make(services).unwrap()))
			}};
		}

		Ok(Arc::new(Self {
			default: create_client!(config, services; base(config)?
				.dns_resolver2(Arc::clone(&services.resolver.resolver))),

			url_preview: create_client!(config, services; {
				let url_preview_bind_addr = config
					.url_preview_bound_interface
					.clone()
					.and_then(Either::left);

				let url_preview_bind_iface = config
					.url_preview_bound_interface
					.clone()
					.and_then(Either::right);

				base(config)
				.and_then(|builder| {
					builder_interface(builder, url_preview_bind_iface.as_deref())
				})?
				.local_address(url_preview_bind_addr)
				.dns_resolver2(Arc::clone(&services.resolver.resolver))
				.redirect(redirect::Policy::limited(3))
			}),

			extern_media: create_client!(config, services; base(config)?
				.dns_resolver2(Arc::clone(&services.resolver.resolver))
				.redirect(redirect::Policy::limited(3))),

			well_known: create_client!(config, services; base(config)?
				.dns_resolver2(Arc::clone(&services.resolver.resolver))
				.connect_timeout(Duration::from_secs(config.well_known_conn_timeout))
				.read_timeout(Duration::from_secs(config.well_known_timeout))
				.timeout(Duration::from_secs(config.well_known_timeout))
				.pool_max_idle_per_host(0)
				.redirect(redirect::Policy::limited(4))),

			federation: create_client!(config, services; base(config)?
				.dns_resolver2(Arc::clone(&services.resolver.resolver.hooked))
				.read_timeout(Duration::from_secs(config.federation_timeout))
				.pool_max_idle_per_host(config.federation_idle_per_host.into())
				.pool_idle_timeout(Duration::from_secs(config.federation_idle_timeout))
				.redirect(redirect::Policy::limited(3))),

			synapse: create_client!(config, services; base(config)?
				.dns_resolver2(Arc::clone(&services.resolver.resolver.hooked))
				.read_timeout(Duration::from_secs(305))
				.pool_max_idle_per_host(0)
				.redirect(redirect::Policy::limited(3))),

			sender: create_client!(config, services; base(config)?
				.dns_resolver2(Arc::clone(&services.resolver.resolver.hooked))
				.read_timeout(Duration::from_secs(config.sender_timeout))
				.timeout(Duration::from_secs(config.sender_timeout))
				.pool_max_idle_per_host(1)
				.pool_idle_timeout(Duration::from_secs(config.sender_idle_timeout))
				.redirect(redirect::Policy::limited(2))),

			appservice: create_client!(config, services; base(config)?
				.dns_resolver2(appservice_resolver(&services))
				.connect_timeout(Duration::from_secs(5))
				.read_timeout(Duration::from_secs(config.appservice_timeout))
				.timeout(Duration::from_secs(config.appservice_timeout))
				.pool_max_idle_per_host(1)
				.pool_idle_timeout(Duration::from_secs(config.appservice_idle_timeout))
				.redirect(redirect::Policy::limited(2))),

			pusher: create_client!(config, services; base(config)?
				.dns_resolver2(Arc::clone(&services.resolver.resolver))
				.pool_max_idle_per_host(1)
				.pool_idle_timeout(Duration::from_secs(config.pusher_idle_timeout))
				.redirect(redirect::Policy::limited(2))),

			oauth: create_client!(config, services; base(config)?
				.dns_resolver2(Arc::clone(&services.resolver.resolver))
				.redirect(redirect::Policy::limited(0))
				.pool_max_idle_per_host(1)),

			cidr_range_denylist: config
				.ip_range_denylist
				.iter()
				.map(IPAddress::parse)
				.inspect(|cidr| trace!("Denied CIDR range: {cidr:?}"))
				.collect::<Result<_, String>>()
				.map_err(|e| err!(Config("ip_range_denylist", e)))?,
		}))
	}

	fn name(&self) -> &str { service::make_name(std::module_path!()) }
}

fn base(config: &Config) -> Result<reqwest::ClientBuilder> {
	let mut builder = reqwest::Client::builder()
		.hickory_dns(true)
		.connect_timeout(Duration::from_secs(config.request_conn_timeout))
		.read_timeout(Duration::from_secs(config.request_timeout))
		.timeout(Duration::from_secs(config.request_total_timeout))
		.pool_idle_timeout(Duration::from_secs(config.request_idle_timeout))
		.pool_max_idle_per_host(config.request_idle_per_host.into())
		.user_agent(tuwunel_core::version::user_agent())
		.redirect(redirect::Policy::limited(6))
		.danger_accept_invalid_certs(config.allow_invalid_tls_certificates)
		.connection_verbose(cfg!(debug_assertions));

	#[cfg(feature = "gzip_compression")]
	{
		builder = if config.gzip_compression {
			builder.gzip(true)
		} else {
			builder.gzip(false).no_gzip()
		};
	};

	#[cfg(feature = "brotli_compression")]
	{
		builder = if config.brotli_compression {
			builder.brotli(true)
		} else {
			builder.brotli(false).no_brotli()
		};
	};

	#[cfg(feature = "zstd_compression")]
	{
		builder = if config.zstd_compression {
			builder.zstd(true)
		} else {
			builder.zstd(false).no_zstd()
		};
	};

	#[cfg(not(feature = "gzip_compression"))]
	{
		builder = builder.no_gzip();
	};

	#[cfg(not(feature = "brotli_compression"))]
	{
		builder = builder.no_brotli();
	};

	#[cfg(not(feature = "zstd_compression"))]
	{
		builder = builder.no_zstd();
	};

	match config.proxy.to_proxy()? {
		| Some(proxy) => Ok(builder.proxy(proxy)),
		| _ => Ok(builder),
	}
}

#[cfg(any(
	target_os = "android",
	target_os = "fuchsia",
	target_os = "linux"
))]
fn builder_interface(
	builder: reqwest::ClientBuilder,
	config: Option<&str>,
) -> Result<reqwest::ClientBuilder> {
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
fn builder_interface(
	builder: reqwest::ClientBuilder,
	config: Option<&str>,
) -> Result<reqwest::ClientBuilder> {
	use tuwunel_core::Err;

	if let Some(iface) = config {
		Err!("Binding to network-interface {iface:?} by name is not supported on this platform.")
	} else {
		Ok(builder)
	}
}

fn appservice_resolver(services: &Arc<OnceServices>) -> Arc<dyn Resolve> {
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
