use std::{
	ops::Deref,
	sync::{Arc, LazyLock},
	time::Duration,
};

use ipaddress::IPAddress;
use reqwest::{Certificate, Client, ClientBuilder, dns::Resolve, redirect};
use tuwunel_core::{Config, Result, either::Either, err, implement, trace};

use crate::{Services, service};

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

	pub cidr_range_denylist: Vec<IPAddress>,
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
				.collect::<Result<_, String>>()
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

		url_preview: with!("preview", cb => {
			let interface = &services
				.config
				.url_preview_bound_interface;

			let bind_addr = interface.clone().and_then(Either::left);
			let bind_iface = interface.clone().and_then(Either::right);

			builder_interface(cb, bind_iface.as_deref())?
				.local_address(bind_addr)
				.dns_resolver(Arc::clone(&services.resolver.resolver))
				.redirect(redirect::Policy::limited(3))
		}),

		extern_media: with!(cb => cb
			.dns_resolver(Arc::clone(&services.resolver.resolver))
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
			.dns_resolver(Arc::clone(&services.resolver.resolver))
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

fn base(config: &Config, name: Option<&str>) -> Result<ClientBuilder> {
	let mut user_agent = tuwunel_core::version::user_agent();
	let user_agent_with_name;
	if let Some(name) = name {
		user_agent_with_name = format!("{user_agent} {name}");
		user_agent = &user_agent_with_name;
	}

	let mut builder = Client::builder()
		.hickory_dns(true)
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

	if let Some(iface) = config {
		Err!("Binding to network-interface {iface:?} by name is not supported on this platform.")
	} else {
		Ok(builder)
	}
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
