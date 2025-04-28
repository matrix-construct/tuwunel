use std::{net::SocketAddr, sync::Arc, time::Duration};

use futures::FutureExt;
use hickory_resolver::{TokioResolver, lookup_ip::LookupIp};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tuwunel_core::{Result, Server, err};

use super::cache::{Cache, CachedOverride};

pub struct Resolver {
	pub(crate) resolver: Arc<TokioResolver>,
	pub(crate) hooked: Arc<Hooked>,
	server: Arc<Server>,
}

pub(crate) struct Hooked {
	resolver: Arc<TokioResolver>,
	cache: Arc<Cache>,
	server: Arc<Server>,
}

type ResolvingResult = Result<Addrs, Box<dyn std::error::Error + Send + Sync>>;

impl Resolver {
	#[allow(clippy::as_conversions, clippy::cast_sign_loss, clippy::cast_possible_truncation)]
	pub(super) fn build(server: &Arc<Server>, cache: Arc<Cache>) -> Result<Arc<Self>> {
		let config = &server.config;
		let (sys_conf, mut opts) = hickory_resolver::system_conf::read_system_conf()
			.map_err(|e| err!(error!("Failed to configure DNS resolver from system: {e}")))?;

		let mut conf = hickory_resolver::config::ResolverConfig::new();

		if let Some(domain) = sys_conf.domain() {
			conf.set_domain(domain.clone());
		}

		for sys_conf in sys_conf.search() {
			conf.add_search(sys_conf.clone());
		}

		for sys_conf in sys_conf.name_servers() {
			let mut ns = sys_conf.clone();

			if config.query_over_tcp_only {
				ns.protocol = hickory_resolver::proto::xfer::Protocol::Tcp;
			}

			ns.trust_negative_responses = !config.query_all_nameservers;

			conf.add_name_server(ns);
		}

		opts.cache_size = config.dns_cache_entries as usize;
		opts.preserve_intermediates = true;
		opts.negative_min_ttl = Some(Duration::from_secs(config.dns_min_ttl_nxdomain));
		opts.negative_max_ttl = Some(Duration::from_secs(60 * 60 * 24 * 30));
		opts.positive_min_ttl = Some(Duration::from_secs(config.dns_min_ttl));
		opts.positive_max_ttl = Some(Duration::from_secs(60 * 60 * 24 * 7));
		opts.timeout = Duration::from_secs(config.dns_timeout);
		opts.attempts = config.dns_attempts as usize;
		opts.try_tcp_on_error = config.dns_tcp_fallback;
		opts.num_concurrent_reqs = 1;
		opts.edns0 = true;
		opts.case_randomization = true;
		opts.ip_strategy = match config.ip_lookup_strategy {
			| 1 => hickory_resolver::config::LookupIpStrategy::Ipv4Only,
			| 2 => hickory_resolver::config::LookupIpStrategy::Ipv6Only,
			| 3 => hickory_resolver::config::LookupIpStrategy::Ipv4AndIpv6,
			| 4 => hickory_resolver::config::LookupIpStrategy::Ipv6thenIpv4,
			| _ => hickory_resolver::config::LookupIpStrategy::Ipv4thenIpv6,
		};

		let rt_prov = hickory_resolver::proto::runtime::TokioRuntimeProvider::new();
		let conn_prov = hickory_resolver::name_server::TokioConnectionProvider::new(rt_prov);
		let mut builder = TokioResolver::builder_with_config(conf, conn_prov);
		*builder.options_mut() = opts;
		let resolver = Arc::new(builder.build());

		Ok(Arc::new(Self {
			resolver: resolver.clone(),
			hooked: Arc::new(Hooked { resolver, cache, server: server.clone() }),
			server: server.clone(),
		}))
	}

	/// Clear the in-memory hickory-dns caches
	#[inline]
	pub fn clear_cache(&self) {
		self.resolver.clear_cache();
	}
}

impl Resolve for Resolver {
	fn resolve(&self, name: Name) -> Resolving {
		resolve_to_reqwest(self.server.clone(), self.resolver.clone(), name).boxed()
	}
}

impl Resolve for Hooked {
	fn resolve(&self, name: Name) -> Resolving {
		hooked_resolve(self.cache.clone(), self.server.clone(), self.resolver.clone(), name)
			.boxed()
	}
}

#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(name = ?name.as_str())
)]
async fn hooked_resolve(
	cache: Arc<Cache>,
	server: Arc<Server>,
	resolver: Arc<TokioResolver>,
	name: Name,
) -> Result<Addrs, Box<dyn std::error::Error + Send + Sync>> {
	match cache.get_override(name.as_str()).await {
		| Ok(cached) if cached.valid() => cached_to_reqwest(cached).await,
		| Ok(CachedOverride { overriding, .. }) if overriding.is_some() => {
			resolve_to_reqwest(
				server,
				resolver,
				overriding
					.as_deref()
					.map(str::parse)
					.expect("overriding is set for this record")
					.expect("overriding is a valid internet name"),
			)
			.boxed()
			.await
		},

		| _ => {
			resolve_to_reqwest(server, resolver, name)
				.boxed()
				.await
		},
	}
}

async fn resolve_to_reqwest(
	server: Arc<Server>,
	resolver: Arc<TokioResolver>,
	name: Name,
) -> ResolvingResult {
	use std::{io, io::ErrorKind::Interrupted};

	let handle_shutdown = || Box::new(io::Error::new(Interrupted, "Server shutting down"));
	let handle_results = |results: LookupIp| {
		Box::new(
			results
				.into_iter()
				.map(|ip| SocketAddr::new(ip, 0)),
		)
	};

	tokio::select! {
		results = resolver.lookup_ip(name.as_str()) => Ok(handle_results(results?)),
		() = server.until_shutdown() => Err(handle_shutdown()),
	}
}

async fn cached_to_reqwest(cached: CachedOverride) -> ResolvingResult {
	let addrs = cached
		.ips
		.into_iter()
		.map(move |ip| SocketAddr::new(ip, cached.port));

	Ok(Box::new(addrs))
}
