use std::{
	fmt,
	net::{IpAddr, SocketAddr},
};

use serde::{Deserialize, Serialize};
use tuwunel_core::{arrayvec::ArrayString, smallstr::SmallString, utils::math::Expected};

use super::DestString;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum FedDest {
	Literal(SocketAddr),
	Named(HostString, PortString),
}

/// FedDest::Named host domain
pub(super) type HostString = SmallString<[u8; 32]>;

/// FedDest::Named numeric or service-name
pub(super) type PortString = ArrayString<16>;

const DEFAULT_PORT: &str = ":8448";

pub(crate) fn get_ip_with_port(dest_str: &str) -> Option<FedDest> {
	if let Ok(dest) = dest_str.parse::<SocketAddr>() {
		Some(FedDest::Literal(dest))
	} else if let Ok(ip_addr) = dest_str.parse::<IpAddr>() {
		Some(FedDest::Literal(SocketAddr::new(ip_addr, 8448)))
	} else {
		None
	}
}

pub(crate) fn add_port_to_hostname(dest: &str) -> FedDest {
	let (host, port) = match dest.find(':') {
		| None => (dest, DEFAULT_PORT),
		| Some(pos) => dest.split_at(pos),
	};

	FedDest::Named(
		host.into(),
		PortString::from(port).unwrap_or_else(|_| FedDest::default_port()),
	)
}

impl FedDest {
	pub(crate) fn https_string(&self) -> DestString {
		match self {
			| Self::Literal(addr) => format!("https://{addr}").into(),
			| Self::Named(host, port) => format!("https://{host}{port}").into(),
		}
	}

	pub(crate) fn uri_string(&self) -> DestString {
		match self {
			| Self::Literal(addr) => addr.to_string().into(),
			| Self::Named(host, port) => [host.as_str(), port.as_str()].concat().into(),
		}
	}

	#[inline]
	pub(crate) fn hostname(&self) -> HostString {
		match &self {
			| Self::Literal(addr) => addr.ip().to_string().into(),
			| Self::Named(host, _) => host.clone(),
		}
	}

	#[inline]
	#[expect(clippy::string_slice)]
	pub(crate) fn port(&self) -> Option<u16> {
		match &self {
			| Self::Literal(addr) => Some(addr.port()),
			| Self::Named(_, port) => port[1..].parse().ok(),
		}
	}

	#[inline]
	#[must_use]
	pub fn default_port() -> PortString {
		PortString::from(DEFAULT_PORT).expect("default port string")
	}

	#[inline]
	#[must_use]
	pub fn size(&self) -> usize {
		match self {
			| Self::Literal(saddr) => size_of_val(saddr),
			| Self::Named(host, port) => host.len().expected_add(port.capacity()),
		}
	}
}

impl fmt::Display for FedDest {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.uri_string().as_str())
	}
}
