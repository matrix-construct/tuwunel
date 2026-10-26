use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use either::{
	Either,
	Either::{Left, Right},
};
use ruma::ServerName;
use serde::Deserialize;

use crate::{
	Err, Result,
	arrayvec::ArrayString,
	err, implement,
	utils::{BoolExt, string::to_array_string},
};

type PermissionDigits = ArrayString<10>;

#[derive(Deserialize, Clone, Debug)]
#[serde(transparent)]
pub(super) struct ListeningPort {
	#[serde(with = "either::serde_untagged")]
	pub(super) ports: Either<u16, Vec<u16>>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(transparent)]
pub(super) struct ListeningAddr {
	#[serde(with = "either::serde_untagged")]
	pub(super) addrs: Either<IpAddr, Vec<IpAddr>>,
}

/// Interprets the configured Unix socket permission digits as an octal mode.
///
/// The returned value is suitable for applying to a newly created socket. An
/// invalid octal digit produces a configuration error.
#[implement(super::Config)]
pub fn get_unix_socket_perms(&self) -> Result<u32> {
	let octal_perms: PermissionDigits = to_array_string(self.http.listen.unix_socket_perms);
	let socket_perms = u32::from_str_radix(&octal_perms, 8).map_err(|_| {
		err!(Config("http.listen.unix_socket_perms", "failed to convert octal permissions"))
	})?;

	Ok(socket_perms)
}

/// Builds every configured TCP listener address.
///
/// The result is the Cartesian product of the configured hosts and ports. When
/// neither TCP addresses nor a Unix socket are configured, loopback hosts
/// apply.
#[must_use]
#[implement(super::Config)]
pub fn get_bind_addrs(&self) -> Vec<SocketAddr> {
	let mut addrs = Vec::with_capacity(
		self.get_bind_hosts()
			.len()
			.saturating_mul(self.get_bind_ports().len()),
	);
	for host in &self.get_bind_hosts() {
		for port in &self.get_bind_ports() {
			addrs.push(SocketAddr::new(*host, *port));
		}
	}

	addrs
}

#[implement(super::Config)]
fn get_bind_hosts(&self) -> Vec<IpAddr> {
	self.http
		.listen
		.address
		.as_ref()
		.map(|address| match &address.addrs {
			| Left(addr) => vec![*addr],
			| Right(addrs) => addrs.clone(),
		})
		.unwrap_or_else(|| {
			self.http
				.listen
				.unix_socket_path
				.is_none()
				.then(|| vec![Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()])
				.unwrap_or_default()
		})
}

#[implement(super::Config)]
fn get_bind_ports(&self) -> Vec<u16> {
	match &self.http.listen.port.ports {
		| Left(port) => vec![*port],
		| Right(ports) => ports.clone(),
	}
}

/// Whether the addresses to bind come from the built-in default rather than
/// the `http.listen.address` option.
///
/// The default is a guess that both loopback families exist on the host, so
/// one of them failing to bind is tolerable where a configured address is not.
#[implement(super::Config)]
#[inline]
#[must_use]
pub fn is_address_defaulted(&self) -> bool { self.http.listen.address.is_none() }

/// Refuses a request that needs federation while federation is disabled.
///
/// Inbound and outbound requests fail with the same 403 `M_FORBIDDEN`, since
/// the refusal enforces the operator's setting rather than reporting a fault.
#[implement(super::Config)]
#[inline]
pub fn require_federation(&self) -> Result {
	self.allow_federation
		.into_option()
		.ok_or_else(|| err!(Request(Forbidden("Federation is disabled."))))
}

/// Refuses federation with a remote server while federation is disabled or
/// the configuration forbids that server.
///
/// The forbidden-server check always permits the local server name. Either
/// refusal fails with the same 403 `M_FORBIDDEN` for inbound and outbound
/// requests.
#[implement(super::Config)]
pub fn require_remote_permitted(&self, server_name: &ServerName) -> Result {
	self.require_federation()?;

	if self.is_forbidden_remote_server_name(server_name) {
		return Err!(Request(Forbidden(debug_warn!(
			"Federation with {server_name} is not allowed."
		))));
	}

	Ok(())
}

/// Determines whether a remote server name is forbidden.
///
/// The local server name is always permitted. Otherwise a deny-list match or an
/// active allow-list miss forbids the destination.
#[implement(super::Config)]
#[must_use]
pub fn is_forbidden_remote_server_name(&self, server_name: &ServerName) -> bool {
	if server_name == self.server_name {
		return false;
	}

	let deny_list_active = self
		.forbidden_remote_server_names
		.is_empty()
		.is_false();

	let allow_list_active = self
		.allowed_remote_server_names_experimental
		.is_empty()
		.is_false();

	if deny_list_active
		&& self
			.forbidden_remote_server_names
			.is_match(server_name.host())
	{
		return true;
	}

	if allow_list_active
		&& !self
			.allowed_remote_server_names_experimental
			.is_match(server_name.host())
	{
		return true;
	}

	false
}
