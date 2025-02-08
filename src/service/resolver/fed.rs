use std::{
	borrow::Cow,
	fmt,
	net::{IpAddr, SocketAddr},
};

use conduwuit::{arrayvec::ArrayString, utils::math::Expected};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum FedDest {
	Literal(SocketAddr),
	Named(String, PortString),
}

/// numeric or service-name
pub type PortString = ArrayString<16>;

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
		host.to_owned(),
		PortString::from(port).unwrap_or_else(|_| FedDest::default_port()),
	)
}

impl FedDest {
	pub(crate) fn https_string(&self) -> String {
		match self {
			| Self::Literal(addr) => format!("https://{addr}"),
			| Self::Named(host, port) => format!("https://{host}{port}"),
		}
	}

	pub(crate) fn uri_string(&self) -> String {
		match self {
			| Self::Literal(addr) => addr.to_string(),
			| Self::Named(host, port) => format!("{host}{port}"),
		}
	}

	#[inline]
	pub(crate) fn hostname(&self) -> Cow<'_, str> {
		match &self {
			| Self::Literal(addr) => addr.ip().to_string().into(),
			| Self::Named(host, _) => host.into(),
		}
	}

	#[inline]
	#[allow(clippy::string_slice)]
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
