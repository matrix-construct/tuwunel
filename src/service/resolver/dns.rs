use std::{
	io,
	io::ErrorKind::PermissionDenied,
	net::{IpAddr, SocketAddr},
	sync::Arc,
	time::Duration,
};

use futures::FutureExt;
use hickory_resolver::{
	TokioResolver,
	config::{LookupIpStrategy, NameServerConfig, ProtocolConfig, ResolverConfig, ResolverOpts},
	lookup_ip::LookupIp,
	net::runtime::TokioRuntimeProvider,
	system_conf::read_system_conf,
};
use ipaddress::IPAddress;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tuwunel_core::{Result, Server, err, trace};

use super::cache::{Cache, CachedOverride};
use crate::client::ipaddress_from_std;

pub struct Resolver {
	pub(crate) resolver: Arc<TokioResolver>,
	pub(crate) passthru: Arc<Passthru>,
	pub(crate) hooked: Arc<Hooked>,
	server: Arc<Server>,
}

/// Inner resolver wrapper that drops addresses matching the configured CIDR
/// denylist before reqwest opens a connection.
pub struct Validating<R> {
	inner: Arc<R>,
	denylist: Arc<[IPAddress]>,
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
		opts.cache_size = config.dns_cache_entries.into();
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
		let mut builder =
			TokioResolver::builder_with_config(conf, TokioRuntimeProvider::default());
		*builder.options_mut() = Self::configure_opts(server, opts);

		builder
			.build()
			.map(Arc::new)
			.map_err(|e| err!(error!("Failed to build DNS resolver: {e}")))
	}

	fn configure(server: &Arc<Server>) -> Result<(ResolverConfig, ResolverOpts)> {
		let config = &server.config;

		let (base_conf, opts) = if config.dns_servers.is_empty() {
			read_system_conf().map_err(|e| {
				err!(error!("Failed to configure DNS resolver from `/etc/resolv.conf': {e}"))
			})?
		} else {
			(Self::configure_custom(&config.dns_servers)?, ResolverOpts::default())
		};

		let name_servers = base_conf
			.name_servers()
			.iter()
			.cloned()
			.map(|mut ns| {
				ns.trust_negative_responses = !config.query_all_nameservers;
				if config.query_over_tcp_only {
					ns.connections
						.retain(|conn| matches!(conn.protocol, ProtocolConfig::Tcp));
				}

				ns
			})
			.collect();

		let conf = ResolverConfig::from_parts(
			base_conf.domain().cloned(),
			base_conf.search().to_vec(),
			name_servers,
		);

		Ok((conf, opts))
	}

	fn configure_custom(servers: &[String]) -> Result<ResolverConfig> {
		let name_servers = servers
			.iter()
			.map(String::as_str)
			.map(Self::parse_nameserver)
			.collect::<Result<_>>()?;

		Ok(ResolverConfig::from_parts(None, vec![], name_servers))
	}

	pub(super) fn parse_nameserver(server: &str) -> Result<NameServerConfig> {
		let (ip, port) = server
			.parse::<SocketAddr>()
			.map(|addr| (addr.ip(), addr.port()))
			.or_else(|_| server.parse::<IpAddr>().map(|ip| (ip, 53)))
			.map_err(|e| {
				err!(Config(
					"dns_servers",
					"{server:?} is not an IP address or socket address: {e}"
				))
			})?;

		let mut conf = NameServerConfig::udp_and_tcp(ip);
		for connection in &mut conf.connections {
			connection.port = port;
		}

		Ok(conf)
	}

	#[expect(clippy::as_conversions)]
	fn configure_opts(server: &Arc<Server>, mut opts: ResolverOpts) -> ResolverOpts {
		let config = &server.config;

		opts.negative_max_ttl = Some(Duration::from_hours(720));
		opts.positive_max_ttl = Some(Duration::from_hours(168));
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

impl<R: Resolve + 'static> Validating<R> {
	pub fn new(inner: Arc<R>, denylist: Arc<[IPAddress]>) -> Arc<Self> {
		Arc::new(Self { inner, denylist })
	}
}

impl<R: Resolve + 'static> Resolve for Validating<R> {
	fn resolve(&self, name: Name) -> Resolving {
		validate_addrs(self.inner.clone(), self.denylist.clone(), name).boxed()
	}
}

async fn validate_addrs<R: Resolve + 'static>(
	inner: Arc<R>,
	denylist: Arc<[IPAddress]>,
	name: Name,
) -> ResolvingResult {
	let mut filtered = inner
		.resolve(name)
		.await?
		.filter(move |sa| {
			let ip = ipaddress_from_std(sa.ip());
			!denylist.iter().any(|cidr| cidr.includes(&ip))
		})
		.peekable();

	if filtered.peek().is_none() {
		return Err(Box::new(io::Error::new(
			PermissionDenied,
			"All resolved addresses are denied by ip_range_denylist",
		)));
	}

	Ok(Box::new(filtered))
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

	let handle_results = |results: LookupIp| -> Addrs {
		let addrs = results
			.iter()
			.map(|ip| SocketAddr::new(ip, 0))
			.collect::<Vec<_>>()
			.into_iter();

		Box::new(addrs)
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
