use std::{fmt::Debug, net::IpAddr};

use futures::{FutureExt, TryFutureExt, future::ready};
use hickory_resolver::{
	net::{DnsError, NetError},
	proto::rr::{Name, RData, rdata::SRV},
};
use ruma::ServerName;
use tuwunel_core::{
	Err, Result, debug, debug_info, debug_warn, err, error, format_array_string, implement,
	trace, utils::string::to_small_string,
};

use super::{
	DestString, FedDest,
	cache::{CachedDest, CachedOverride, MAX_IPS},
	fed::{HostString, PortString, add_port_to_hostname, get_ip_with_port, unbracket},
};

#[derive(Clone, Debug)]
pub(crate) struct ActualDest {
	pub(crate) dest: FedDest,
	pub(crate) host: DestString,
	pub(crate) srv: bool,
}

impl ActualDest {
	#[inline]
	pub(crate) fn to_string(&self) -> DestString { self.dest.https_string() }
}

impl From<CachedDest> for ActualDest {
	fn from(CachedDest { dest, host, srv, .. }: CachedDest) -> Self { Self { dest, host, srv } }
}

#[implement(ActualDest)]
fn direct(dest: FedDest, host: &str) -> Self {
	Self {
		dest,
		host: Self::dest_host(host).uri_string(),
		srv: false,
	}
}

#[implement(ActualDest)]
fn via_srv(dest: FedDest, host: &str) -> Self { Self { srv: true, ..Self::direct(dest, host) } }

#[implement(ActualDest)]
fn dest_host(host: &str) -> FedDest {
	get_ip_with_port(host).unwrap_or_else(|| add_port_to_hostname(host))
}

#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "debug", name = "resolve")]
pub(crate) async fn get_actual_dest(&self, server_name: &ServerName) -> Result<ActualDest> {
	self.lookup_actual_dest_with_policy(server_name, false)
		.map_ok(|(cached, _)| cached.into())
		.await
}

#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "debug", name = "resolve")]
pub(crate) async fn get_actual_dest_allow_self(
	&self,
	server_name: &ServerName,
) -> Result<ActualDest> {
	self.lookup_actual_dest_with_policy(server_name, true)
		.map_ok(|(cached, _)| cached.into())
		.await
}

#[implement(super::Service)]
async fn lookup_actual_dest_with_policy(
	&self,
	server_name: &ServerName,
	allow_self: bool,
) -> Result<(CachedDest, bool)> {
	self.validate_self_destination(server_name, allow_self)?;

	if let Ok(result) = self.cache.get_destination(server_name).await {
		return Ok((result, true));
	}

	let _dedup = self.resolving.lock(server_name).await;
	if let Ok(result) = self.cache.get_destination(server_name).await {
		return Ok((result, true));
	}

	self.validate_dest_address(server_name)?;

	self.resolve_actual_dest_unchecked(server_name, true)
		.inspect_ok(|result| self.cache.set_destination(server_name, result))
		.map_ok(|result| (result, false))
		.boxed()
		.await
}

/// Returns: `actual_destination`, host header
/// Implemented according to the specification at <https://matrix.org/docs/spec/server_server/r0.1.4#resolving-server-names>
/// Numbers in comments below refer to bullet points in linked section of
/// specification
#[implement(super::Service)]
pub async fn resolve_actual_dest(&self, dest: &ServerName, cache: bool) -> Result<CachedDest> {
	self.validate_dest(dest, false)?;
	self.resolve_actual_dest_unchecked(dest, cache)
		.await
}

#[implement(super::Service)]
#[tracing::instrument(name = "actual", level = "debug", skip(self, cache))]
async fn resolve_actual_dest_unchecked(
	&self,
	dest: &ServerName,
	cache: bool,
) -> Result<CachedDest> {
	let ActualDest { dest: actual, host, srv } = self.actual_dest(dest, cache).await?;

	debug!(?actual, ?host, srv, "Actual destination");
	Ok(CachedDest {
		dest: actual,
		host,
		expire: CachedDest::default_expire(),
		srv,
	})
}

#[implement(super::Service)]
async fn actual_dest(&self, dest: &ServerName, cache: bool) -> Result<ActualDest> {
	let name = dest.as_str();
	let direct = |actual| ActualDest::direct(actual, name);

	match get_ip_with_port(name) {
		| Some(host_port) => Self::actual_dest_1(host_port).map(direct),
		| None if name.contains(':') =>
			self.actual_dest_2(dest, cache)
				.map_ok(direct)
				.await,
		| None => self.actual_dest_named(dest, cache).await,
	}
}

#[implement(super::Service)]
fn actual_dest_1(host_port: FedDest) -> Result<FedDest> {
	debug!("1: IP literal with provided or default port");
	Ok(host_port)
}

#[implement(super::Service)]
async fn actual_dest_2(&self, dest: &ServerName, cache: bool) -> Result<FedDest> {
	debug!("2: Hostname with included port");
	self.direct_route(dest.as_str(), cache).await
}

#[implement(super::Service)]
async fn actual_dest_named(&self, dest: &ServerName, cache: bool) -> Result<ActualDest> {
	let name = dest.as_str();

	self.services.server.check_running()?;
	match self.request_well_known(name).await? {
		| Some(delegated) => self.actual_dest_3(cache, &delegated).await,
		| None => match self.query_srv_record(name).await? {
			| Some(overrider) =>
				self.actual_dest_4(name, cache, overrider)
					.map_ok(|actual| ActualDest::via_srv(actual, name))
					.await,
			| None =>
				self.actual_dest_5(dest, cache)
					.map_ok(|actual| ActualDest::direct(actual, name))
					.await,
		},
	}
}

#[implement(super::Service)]
pub(super) async fn actual_dest_3(&self, cache: bool, delegated: &str) -> Result<ActualDest> {
	debug!("3: A .well-known file is available");
	let direct = |actual| ActualDest::direct(actual, delegated);

	match get_ip_with_port(delegated) {
		| Some(host_and_port) => Self::actual_dest_3_1(host_and_port).map(direct),
		| None if delegated.contains(':') =>
			self.actual_dest_3_2(cache, delegated)
				.map_ok(direct)
				.await,
		| None => match self.query_srv_record(delegated).await? {
			| Some(overrider) =>
				self.actual_dest_3_3(cache, delegated, overrider)
					.map_ok(|actual| ActualDest::via_srv(actual, delegated))
					.await,
			| None =>
				self.actual_dest_3_4(cache, delegated)
					.map_ok(direct)
					.await,
		},
	}
}

#[cfg(test)]
#[implement(super::Service)]
pub(crate) async fn test_delegated_dest(&self, delegated: &str) -> Result<ActualDest> {
	self.actual_dest_3(false, delegated).await
}

#[implement(super::Service)]
fn actual_dest_3_1(host_and_port: FedDest) -> Result<FedDest> {
	debug!("3.1: IP literal in .well-known file");
	Ok(host_and_port)
}

#[implement(super::Service)]
async fn actual_dest_3_2(&self, cache: bool, delegated: &str) -> Result<FedDest> {
	debug!("3.2: Hostname with port in .well-known file");
	self.direct_route(delegated, cache).await
}

#[implement(super::Service)]
async fn actual_dest_3_3(
	&self,
	cache: bool,
	delegated: &str,
	overrider: FedDest,
) -> Result<FedDest> {
	debug!("3.3: SRV lookup successful");
	self.srv_route(delegated, cache, overrider).await
}

#[implement(super::Service)]
async fn actual_dest_3_4(&self, cache: bool, delegated: &str) -> Result<FedDest> {
	debug!("3.4: No SRV records, just use the hostname from .well-known");
	self.direct_route(delegated, cache).await
}

#[implement(super::Service)]
async fn actual_dest_4(&self, host: &str, cache: bool, overrider: FedDest) -> Result<FedDest> {
	debug!("4: No .well-known; SRV record found");
	self.srv_route(host, cache, overrider).await
}

#[implement(super::Service)]
async fn actual_dest_5(&self, dest: &ServerName, cache: bool) -> Result<FedDest> {
	debug!("5: No SRV record found");
	self.direct_route(dest.as_str(), cache).await
}

#[implement(super::Service)]
async fn direct_route(&self, name: &str, cache: bool) -> Result<FedDest> {
	let dest = add_port_to_hostname(name);

	self.query_direct(&dest.hostname(), cache)
		.map_ok(|()| dest)
		.await
}

#[implement(super::Service)]
async fn srv_route(&self, name: &str, cache: bool, overrider: FedDest) -> Result<FedDest> {
	let force_port = overrider.port();
	self.maybe_query_and_cache_override(
		name,
		&overrider.hostname(),
		force_port.unwrap_or(8448),
		cache,
	)
	.await?;

	if let Some(port) = force_port {
		let port: PortString = format_array_string!(":{port}");

		return Ok(FedDest::Named(name.into(), port));
	}

	Ok(add_port_to_hostname(name))
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
async fn query_direct(&self, hostname: &str, cache: bool) -> Result {
	if !cache {
		return Ok(());
	}

	self.services.server.check_running()?;

	// Warm the DNS cache without publishing a hostname-wide SRV override.
	self.resolver
		.resolver
		.lookup_ip(hostname)
		.map_ok(drop)
		.or_else(|error| ready(Self::handle_resolve_error(&error, hostname)))
		.await
}

#[implement(super::Service)]
#[inline]
async fn maybe_query_and_cache_override(
	&self,
	untername: &str,
	hostname: &str,
	port: u16,
	cache: bool,
) -> Result {
	if !cache {
		return Ok(());
	}

	if self.cache.has_override(untername, hostname).await {
		return Ok(());
	}

	self.query_and_cache_override(untername, hostname, port)
		.await
}

#[implement(super::Service)]
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
			debug_info!(?untername, ?hostname, "Overriding hostname");
			self.cache
				.set_override(untername, &CachedOverride {
					ips: override_ip.iter().take(MAX_IPS).collect(),
					port,
					expire: CachedOverride::default_expire(),
					overriding: Some(hostname.into()),
				});

			Ok(())
		},
	}
}

#[implement(super::Service)]
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
				let srv = result
					.answers()
					.iter()
					.find_map(|r| match &r.data {
						| RData::SRV(srv) => Some(srv),
						| _ => None,
					});

				return Ok(srv.map(Self::srv_dest));
			},
		}
	}

	Ok(None)
}

#[implement(super::Service)]
fn srv_dest(srv: &SRV) -> FedDest {
	let host: HostString = to_small_string(&srv.target);
	let port: PortString = format_array_string!(":{}", srv.port);

	FedDest::Named(host.trim_end_matches('.').into(), port)
}

#[implement(super::Service)]
fn handle_resolve_error(e: &NetError, host: &'_ str) -> Result {
	// `NetError::Dns(_)` covers responses returned by the remote side (NXDOMAIN,
	// SERVFAIL, REFUSED, ...) only seen with verbose-logging. Local-origin failures
	// (Timeout, NoConnections, Io, ...) keep their warn/error level so an operator
	// notices when their own resolver is unhealthy.
	match e {
		| NetError::Dns(DnsError::NoRecordsFound(_)) => {
			// Raise to debug_warn if we can find out the result wasn't from cache
			debug!(%host, "No DNS records found: {e}");
			Ok(())
		},
		| NetError::Dns(_) => {
			debug_warn!(%host, "DNS response error: {e}");
			Ok(())
		},
		// Remote-supplied name (server name, delegation or SRV target), not a local fault.
		| NetError::Proto(_) if Name::from_utf8(host).is_err() =>
			Err!(debug_error!(%host, error = %e, "Invalid DNS name")),
		| NetError::Timeout => Err!(warn!(%host, "DNS {e}")),
		| NetError::NoConnections => {
			error!(
				"Your DNS server is overloaded and has ran out of connections. It is strongly \
				 recommended you remediate this issue to ensure proper federation connectivity."
			);

			Err!(error!(%host, "DNS error: {e}"))
		},
		| _ => Err!(error!(%host, "DNS error: {e}")),
	}
}

#[implement(super::Service)]
fn validate_dest(&self, dest: &ServerName, allow_self: bool) -> Result {
	self.validate_self_destination(dest, allow_self)?;
	self.validate_dest_address(dest)
}

#[implement(super::Service)]
fn validate_self_destination(&self, dest: &ServerName, allow_self: bool) -> Result {
	if !allow_self
		&& dest == self.services.server.name
		&& !self.services.server.config.federation_loopback
	{
		return Err!("Won't send federation request to ourselves");
	}

	Ok(())
}

#[implement(super::Service)]
fn validate_dest_address(&self, dest: &ServerName) -> Result {
	if dest.is_ip_literal() || unbracket(dest.host()).parse::<IpAddr>().is_ok() {
		self.validate_dest_ip_literal(dest)?;
	}

	Ok(())
}

#[implement(super::Service)]
fn validate_dest_ip_literal(&self, dest: &ServerName) -> Result {
	trace!("Destination is an IP literal, checking against IP range denylist.",);
	let ip = unbracket(dest.host()).parse().map_err(|e| {
		err!(BadServerResponse(debug_error!("Failed to parse IP literal from string: {e}")))
	})?;

	if !self.services.client.valid_cidr_range_ip(ip) {
		return Err!(BadServerResponse("Not allowed to send requests to this IP"));
	}

	Ok(())
}
