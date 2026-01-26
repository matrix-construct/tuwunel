use std::{
	fmt::Debug,
	net::{IpAddr, SocketAddr},
};

use futures::{FutureExt, TryFutureExt};
use hickory_resolver::ResolveError;
use ipaddress::IPAddress;
use ruma::ServerName;
use tuwunel_core::{Err, Result, debug, debug_info, err, error, trace};

use super::{
	DestString, FedDest,
	cache::{CachedDest, CachedOverride, MAX_IPS},
	fed::{PortString, add_port_to_hostname, get_ip_with_port},
};

#[derive(Clone, Debug)]
pub(crate) struct ActualDest {
	pub(crate) dest: FedDest,
	pub(crate) host: DestString,
}

impl ActualDest {
	#[inline]
	pub(crate) fn to_string(&self) -> DestString { self.dest.https_string() }
}

impl super::Service {
	#[tracing::instrument(skip_all, level = "debug", name = "resolve")]
	pub(crate) async fn get_actual_dest(&self, server_name: &ServerName) -> Result<ActualDest> {
		let (CachedDest { dest, host, .. }, _cached) =
			self.lookup_actual_dest(server_name).await?;

		Ok(ActualDest { dest, host })
	}

	pub(crate) async fn lookup_actual_dest(
		&self,
		server_name: &ServerName,
	) -> Result<(CachedDest, bool)> {
		if let Ok(result) = self.cache.get_destination(server_name).await {
			return Ok((result, true));
		}

		let _dedup = self.resolving.lock(server_name);
		if let Ok(result) = self.cache.get_destination(server_name).await {
			return Ok((result, true));
		}

		self.resolve_actual_dest(server_name, true)
			.inspect_ok(|result| self.cache.set_destination(server_name, result))
			.map_ok(|result| (result, false))
			.boxed()
			.await
	}

	/// Returns: `actual_destination`, host header
	/// Implemented according to the specification at <https://matrix.org/docs/spec/server_server/r0.1.4#resolving-server-names>
	/// Numbers in comments below refer to bullet points in linked section of
	/// specification
	#[tracing::instrument(name = "actual", level = "debug", skip(self, cache))]
	pub async fn resolve_actual_dest(
		&self,
		dest: &ServerName,
		cache: bool,
	) -> Result<CachedDest> {
		self.validate_dest(dest)?;
		let mut host: DestString = dest.as_str().into();
		let actual_dest = match get_ip_with_port(dest.as_str()) {
			| Some(host_port) => Self::actual_dest_1(host_port)?,
			| None =>
				if let Some(pos) = dest.as_str().find(':') {
					self.actual_dest_2(dest, cache, pos).await?
				} else {
					self.conditional_query_and_cache(dest.as_str(), 8448, true)
						.await?;
					self.services.server.check_running()?;
					match self.request_well_known(dest.as_str()).await? {
						| Some(delegated) =>
							self.actual_dest_3(&mut host, cache, &delegated)
								.await?,
						| _ => match self.query_srv_record(dest.as_str()).await? {
							| Some(overrider) =>
								self.actual_dest_4(&host, cache, overrider)
									.await?,
							| _ => self.actual_dest_5(dest, cache).await?,
						},
					}
				},
		};

		// Can't use get_ip_with_port here because we don't want to add a port
		// to an IP address if it wasn't specified
		let host = if let Ok(addr) = host.parse::<SocketAddr>() {
			FedDest::Literal(addr)
		} else if let Ok(addr) = host.parse::<IpAddr>() {
			FedDest::Named(addr.to_string().into(), FedDest::default_port())
		} else if let Some(pos) = host.find(':') {
			let (host, port) = host.split_at(pos);
			FedDest::Named(
				host.into(),
				port.try_into()
					.unwrap_or_else(|_| FedDest::default_port()),
			)
		} else {
			FedDest::Named(host.as_str().into(), FedDest::default_port())
		};

		debug!("Actual destination: {actual_dest:?} hostname: {host:?}");
		Ok(CachedDest {
			dest: actual_dest,
			host: host.uri_string(),
			expire: CachedDest::default_expire(),
		})
	}

	fn actual_dest_1(host_port: FedDest) -> Result<FedDest> {
		debug!("1: IP literal with provided or default port");
		Ok(host_port)
	}

	async fn actual_dest_2(&self, dest: &ServerName, cache: bool, pos: usize) -> Result<FedDest> {
		debug!("2: Hostname with included port");
		let (host, port) = dest.as_str().split_at(pos);
		self.conditional_query_and_cache(host, port.parse::<u16>().unwrap_or(8448), cache)
			.await?;

		Ok(FedDest::Named(
			host.into(),
			port.try_into()
				.unwrap_or_else(|_| FedDest::default_port()),
		))
	}

	async fn actual_dest_3(
		&self,
		host: &mut DestString,
		cache: bool,
		delegated: &str,
	) -> Result<FedDest> {
		debug!("3: A .well-known file is available");
		*host = add_port_to_hostname(delegated).uri_string();
		match get_ip_with_port(delegated) {
			| Some(host_and_port) => Self::actual_dest_3_1(host_and_port),
			| None =>
				if let Some(pos) = delegated.find(':') {
					self.actual_dest_3_2(cache, delegated, pos).await
				} else {
					trace!("Delegated hostname has no port in this branch");
					match self.query_srv_record(delegated).await? {
						| Some(overrider) =>
							self.actual_dest_3_3(cache, delegated, overrider)
								.await,
						| _ => self.actual_dest_3_4(cache, delegated).await,
					}
				},
		}
	}

	fn actual_dest_3_1(host_and_port: FedDest) -> Result<FedDest> {
		debug!("3.1: IP literal in .well-known file");
		Ok(host_and_port)
	}

	async fn actual_dest_3_2(&self, cache: bool, delegated: &str, pos: usize) -> Result<FedDest> {
		debug!("3.2: Hostname with port in .well-known file");
		let (host, port) = delegated.split_at(pos);
		self.conditional_query_and_cache(host, port.parse::<u16>().unwrap_or(8448), cache)
			.await?;

		Ok(FedDest::Named(
			host.into(),
			port.try_into()
				.unwrap_or_else(|_| FedDest::default_port()),
		))
	}

	async fn actual_dest_3_3(
		&self,
		cache: bool,
		delegated: &str,
		overrider: FedDest,
	) -> Result<FedDest> {
		debug!("3.3: SRV lookup successful");
		let force_port = overrider.port();
		self.conditional_query_and_cache_override(
			delegated,
			&overrider.hostname(),
			force_port.unwrap_or(8448),
			cache,
		)
		.await?;

		if let Some(port) = force_port {
			return Ok(FedDest::Named(
				delegated.into(),
				format!(":{port}")
					.as_str()
					.try_into()
					.unwrap_or_else(|_| FedDest::default_port()),
			));
		}

		Ok(add_port_to_hostname(delegated))
	}

	async fn actual_dest_3_4(&self, cache: bool, delegated: &str) -> Result<FedDest> {
		debug!("3.4: No SRV records, just use the hostname from .well-known");
		self.conditional_query_and_cache(delegated, 8448, cache)
			.await?;

		Ok(add_port_to_hostname(delegated))
	}

	async fn actual_dest_4(
		&self,
		host: &str,
		cache: bool,
		overrider: FedDest,
	) -> Result<FedDest> {
		debug!("4: No .well-known; SRV record found");
		let force_port = overrider.port();
		self.conditional_query_and_cache_override(
			host,
			&overrider.hostname(),
			force_port.unwrap_or(8448),
			cache,
		)
		.await?;

		if let Some(port) = force_port {
			let port = format!(":{port}");
			return Ok(FedDest::Named(
				host.into(),
				PortString::from(port.as_str()).unwrap_or_else(|_| FedDest::default_port()),
			));
		}

		Ok(add_port_to_hostname(host))
	}

	async fn actual_dest_5(&self, dest: &ServerName, cache: bool) -> Result<FedDest> {
		debug!("5: No SRV record found");
		self.conditional_query_and_cache(dest.as_str(), 8448, cache)
			.await?;

		Ok(add_port_to_hostname(dest.as_str()))
	}

	#[inline]
	async fn conditional_query_and_cache(
		&self,
		hostname: &str,
		port: u16,
		cache: bool,
	) -> Result {
		self.conditional_query_and_cache_override(hostname, hostname, port, cache)
			.await
	}

	#[inline]
	async fn conditional_query_and_cache_override(
		&self,
		untername: &str,
		hostname: &str,
		port: u16,
		cache: bool,
	) -> Result {
		if !cache {
			return Ok(());
		}

		if self.cache.has_override(untername).await {
			return Ok(());
		}

		self.query_and_cache_override(untername, hostname, port)
			.await
	}

	#[tracing::instrument(name = "ip", level = "debug", skip(self))]
	async fn query_and_cache_override(
		&self,
		untername: &'_ str,
		hostname: &'_ str,
		port: u16,
	) -> Result {
		self.services.server.check_running()?;

		debug!("querying IP for {untername:?} ({hostname:?}:{port})");
		match self
			.resolver
			.resolver
			.lookup_ip(hostname.to_owned())
			.await
		{
			| Err(e) => Self::handle_resolve_error(&e, hostname),
			| Ok(override_ip) => {
				self.cache
					.set_override(untername, &CachedOverride {
						ips: override_ip.into_iter().take(MAX_IPS).collect(),
						port,
						expire: CachedOverride::default_expire(),
						overriding: (hostname != untername)
							.then_some(hostname.into())
							.inspect(|_| debug_info!("{untername:?} overridden by {hostname:?}")),
					});

				Ok(())
			},
		}
	}

	#[tracing::instrument(name = "srv", level = "debug", skip(self))]
	async fn query_srv_record(&self, hostname: &'_ str) -> Result<Option<FedDest>> {
		let hostnames =
			[format!("_matrix-fed._tcp.{hostname}."), format!("_matrix._tcp.{hostname}.")];

		for hostname in hostnames {
			self.services.server.check_running()?;

			debug!("querying SRV for {hostname:?}");
			let hostname = hostname.trim_end_matches('.');
			match self.resolver.resolver.srv_lookup(hostname).await {
				| Err(e) => Self::handle_resolve_error(&e, hostname)?,
				| Ok(result) => {
					return Ok(result.iter().next().map(|result| {
						FedDest::Named(
							result
								.target()
								.to_string()
								.trim_end_matches('.')
								.into(),
							format!(":{}", result.port())
								.as_str()
								.try_into()
								.unwrap_or_else(|_| FedDest::default_port()),
						)
					}));
				},
			}
		}

		Ok(None)
	}

	fn handle_resolve_error(e: &ResolveError, host: &'_ str) -> Result {
		use hickory_resolver::{ResolveErrorKind::Proto, proto::ProtoErrorKind};

		match e.kind() {
			| Proto(e) => match e.kind() {
				| ProtoErrorKind::NoRecordsFound { .. } => {
					// Raise to debug_warn if we can find out the result wasn't from cache
					debug!(%host, "No DNS records found: {e}");
					Ok(())
				},
				| ProtoErrorKind::Timeout => {
					Err!(warn!(%host, "DNS {e}"))
				},
				| ProtoErrorKind::NoConnections => {
					error!(
						"Your DNS server is overloaded and has ran out of connections. It is \
						 strongly recommended you remediate this issue to ensure proper \
						 federation connectivity."
					);

					Err!(error!(%host, "DNS error: {e}"))
				},
				| _ => Err!(error!(%host, "DNS error: {e}")),
			},
			| _ => Err!(error!(%host, "DNS error: {e}")),
		}
	}

	fn validate_dest(&self, dest: &ServerName) -> Result {
		if dest == self.services.server.name && !self.services.server.config.federation_loopback {
			return Err!("Won't send federation request to ourselves");
		}

		if dest.is_ip_literal() || IPAddress::is_valid(dest.host()) {
			self.validate_dest_ip_literal(dest)?;
		}

		Ok(())
	}

	fn validate_dest_ip_literal(&self, dest: &ServerName) -> Result {
		trace!("Destination is an IP literal, checking against IP range denylist.",);
		debug_assert!(
			dest.is_ip_literal() || !IPAddress::is_valid(dest.host()),
			"Destination is not an IP literal."
		);
		let ip = IPAddress::parse(dest.host()).map_err(|e| {
			err!(BadServerResponse(debug_error!("Failed to parse IP literal from string: {e}")))
		})?;

		self.validate_ip(&ip)?;

		Ok(())
	}

	pub(crate) fn validate_ip(&self, ip: &IPAddress) -> Result {
		if !self.services.client.valid_cidr_range(ip) {
			return Err!(BadServerResponse("Not allowed to send requests to this IP"));
		}

		Ok(())
	}
}
