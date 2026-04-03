use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use either::{
	Either,
	Either::{Left, Right},
};
use ruma::ServerName;
use serde::Deserialize;

use crate::{Result, err, implement, utils::BoolExt};

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

#[implement(super::Config)]
pub fn get_unix_socket_perms(&self) -> Result<u32> {
	let octal_perms = self.unix_socket_perms.to_string();
	let socket_perms = u32::from_str_radix(&octal_perms, 8)
		.map_err(|_| err!(Config("unix_socket_perms", "failed to convert octal permissions")))?;

	Ok(socket_perms)
}

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
	if let Some(address) = &self.address {
		match &address.addrs {
			| Left(addr) => vec![*addr],
			| Right(addrs) => addrs.clone(),
		}
	} else if self.unix_socket_path.is_some() {
		vec![]
	} else {
		vec![Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()]
	}
}

#[implement(super::Config)]
fn get_bind_ports(&self) -> Vec<u16> {
	match &self.port.ports {
		| Left(port) => vec![*port],
		| Right(ports) => ports.clone(),
	}
}

#[implement(super::Config)]
#[must_use]
pub fn is_forbidden_remote_server_name(&self, server_name: &ServerName) -> bool {
	let deny_list_active = self
		.forbidden_remote_server_names
		.is_empty()
		.is_false();

	let allow_list_active = self
		.allowed_remote_server_names
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
			.allowed_remote_server_names
			.is_match(server_name.host())
	{
		return true;
	}

	false
}
