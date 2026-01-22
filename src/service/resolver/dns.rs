use std::{net::SocketAddr, sync::Arc, time::Duration};

use futures::FutureExt;
use hickory_resolver::{
	TokioResolver,
	config::{LookupIpStrategy, ResolverConfig, ResolverOpts},
	lookup_ip::LookupIp,
};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tuwunel_core::{Result, Server, err, trace};

use super::cache::{Cache, CachedOverride};

pub struct Resolver {
	pub(crate) resolver: Arc<TokioResolver>,
	pub(crate) passthru: Arc<Passthru>,
	pub(crate) hooked: Arc<Hooked>,
	server: Arc<Server>,
}

pub(crate) struct Hooked {
	resolver: Arc<TokioResolver>,
	passthru: Arc<Passthru>,
	cache: Arc<Cache>,
	server: Arc<Server>,
}

pub(crate) struct Passthru {
	resolver: Arc<TokioResolver>,
	server: Arc<Server>,
}

type ResolvingResult = Result<Addrs, Box<dyn std::error::Error + Send + Sync>>;

impl Resolver {
	pub(super) fn build(server: &Arc<Server>, cache: Arc<Cache>) -> Result<Arc<Self>> {
		let config = &server.config;

		// Create the primary resolver.
		let (conf, mut opts) = Self::configure(server)?;
		opts.negative_min_ttl = Some(Duration::from_secs(config.dns_min_ttl_nxdomain));
		opts.positive_min_ttl = Some(Duration::from_secs(config.dns_min_ttl));
		opts.cache_size = config.dns_cache_entries.try_into()?;
		let resolver = Self::create(server, conf.clone(), opts.clone())?;

		// Create the passthru resolver with modified options.
		let (conf, mut opts) = (conf, opts);
		opts.negative_min_ttl = Some(Duration::ZERO);
		opts.positive_min_ttl = Some(Duration::ZERO);
		opts.cache_size = ResolverOpts::default().cache_size;
		let passthru = Arc::new(Passthru {
			resolver: Self::create(server, conf, opts)?,
			server: server.clone(),
		});

		Ok(Arc::new(Self {
			hooked: Arc::new(Hooked {
				resolver: resolver.clone(),
				passthru: passthru.clone(),
				server: server.clone(),
				cache,
			}),
			server: server.clone(),
			passthru,
			resolver,
		}))
	}

	fn create(
		server: &Arc<Server>,
		conf: ResolverConfig,
		opts: ResolverOpts,
	) -> Result<Arc<TokioResolver>> {
		let rt_prov = hickory_resolver::proto::runtime::TokioRuntimeProvider::new();
		let conn_prov = hickory_resolver::name_server::TokioConnectionProvider::new(rt_prov);
		let mut builder = TokioResolver::builder_with_config(conf, conn_prov);
		*builder.options_mut() = Self::configure_opts(server, opts);

		Ok(Arc::new(builder.build()))
	}

	fn configure(server: &Arc<Server>) -> Result<(ResolverConfig, ResolverOpts)> {
		let config = &server.config;
		let (sys_conf, opts) =
			hickory_resolver::system_conf::read_system_conf().map_err(|e| {
				err!(error!("Failed to configure DNS resolver from `/etc/resolv.conf': {e}"))
			})?;

		let mut conf = ResolverConfig::new();
		if let Some(domain) = sys_conf.domain() {
			conf.set_domain(domain.clone());
		}

		for sys_conf in sys_conf.search() {
			conf.add_search(sys_conf.clone());
		}

		for sys_conf in sys_conf.name_servers() {
			let mut ns = sys_conf.clone();
			ns.trust_negative_responses = !config.query_all_nameservers;
			if config.query_over_tcp_only {
				ns.protocol = hickory_resolver::proto::xfer::Protocol::Tcp;
			}

			conf.add_name_server(ns);
		}

		Ok((conf, opts))
	}

	#[expect(clippy::as_conversions)]
	fn configure_opts(server: &Arc<Server>, mut opts: ResolverOpts) -> ResolverOpts {
		let config = &server.config;

		opts.negative_max_ttl = Some(Duration::from_secs(60 * 60 * 24 * 30));
		opts.positive_max_ttl = Some(Duration::from_secs(60 * 60 * 24 * 7));
		opts.timeout = Duration::from_secs(config.dns_timeout);
		opts.attempts = config.dns_attempts as usize;
		opts.try_tcp_on_error = config.dns_tcp_fallback;
		opts.num_concurrent_reqs = 1;
		opts.edns0 = true;
		opts.case_randomization = config.dns_case_randomization;
		opts.preserve_intermediates = true;
		opts.ip_strategy = match config.ip_lookup_strategy {
			| 1 => LookupIpStrategy::Ipv4Only,
			| 2 => LookupIpStrategy::Ipv6Only,
			| 3 => LookupIpStrategy::Ipv4AndIpv6,
			| 4 => LookupIpStrategy::Ipv6thenIpv4,
			| _ => LookupIpStrategy::Ipv4thenIpv6,
		};

		opts
	}

	/// Clear the in-memory hickory-dns caches
	#[inline]
	pub fn clear_cache(&self) { self.resolver.clear_cache(); }
}

impl Resolve for Resolver {
	fn resolve(&self, name: Name) -> Resolving {
		let resolver = if self
			.server
			.config
			.dns_passthru_domains
			.is_match(name.as_str())
		{
			trace!(?name, "matched to passthru resolver");
			&self.passthru.resolver
		} else {
			trace!(?name, "using primary resolver");
			&self.resolver
		};

		resolve_to_reqwest(self.server.clone(), resolver.clone(), name).boxed()
	}
}

impl Resolve for Hooked {
	fn resolve(&self, name: Name) -> Resolving {
		let resolver = if self
			.server
			.config
			.dns_passthru_domains
			.is_match(name.as_str())
		{
			trace!(?name, "matched to passthru resolver");
			&self.passthru.resolver
		} else {
			trace!(?name, "using hooked resolver");
			&self.resolver
		};

		hooked_resolve(self.cache.clone(), self.server.clone(), resolver.clone(), name).boxed()
	}
}

impl Resolve for Passthru {
	fn resolve(&self, name: Name) -> Resolving {
		trace!(?name, "using passthru resolver");
		resolve_to_reqwest(self.server.clone(), self.resolver.clone(), name).boxed()
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
		| Ok(cached) if cached.valid() => cached_to_reqwest(cached),
		| Ok(CachedOverride { overriding, .. }) if overriding.is_some() =>
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
			.await,

		| _ =>
			resolve_to_reqwest(server, resolver, name)
				.boxed()
				.await,
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

fn cached_to_reqwest(cached: CachedOverride) -> ResolvingResult {
	let addrs = cached
		.ips
		.into_iter()
		.map(move |ip| SocketAddr::new(ip, cached.port));

	Ok(Box::new(addrs))
}
