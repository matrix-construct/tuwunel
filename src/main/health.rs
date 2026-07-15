//! Liveness probe of a running server for `--health-check`.

use std::{
	io::{Read, Write},
	net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream},
	path::PathBuf,
	time::Duration,
};
#[cfg(unix)]
use std::{os::unix::net::UnixStream, path::Path};

use tuwunel_core::{Err, Result, config::Config, itertools::Itertools};

use crate::{Args, args::update};

const REQUEST: &[u8] =
	b"GET /_tuwunel/server_version HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

const TIMEOUT: Duration = Duration::from_secs(5);

/// Probe the listeners of a running server sharing this configuration,
/// exiting zero when one answers.
pub fn check(args: &Args) -> Result {
	let config_paths = args
		.config
		.as_deref()
		.into_iter()
		.flat_map(<[_]>::iter)
		.map(PathBuf::as_path);

	let config = Config::load(config_paths)
		.and_then(|raw| update(raw, args))
		.and_then(|raw| Config::new(&raw))?;

	#[cfg(unix)]
	let unix = config.unix_socket_path.as_deref().map(probe_unix);

	#[cfg(not(unix))]
	let unix: Option<Result> = None;

	let tls = config.tls.get_tls_cert_key().is_some() && !config.tls.dual_protocol;

	let tcp = config
		.get_bind_addrs()
		.into_iter()
		.map(loopback)
		.map(|addr| probe_tcp(addr, tls));

	// The server keeps serving when one listener fails to bind, so healthy means
	// any listener answering.
	unix.into_iter()
		.chain(tcp)
		.find_or_last(Result::is_ok)
		.unwrap_or_else(|| Err!("No listeners are configured."))
}

#[cfg(unix)]
fn probe_unix(path: &Path) -> Result {
	let stream = UnixStream::connect(path)?;
	stream.set_read_timeout(Some(TIMEOUT))?;
	stream.set_write_timeout(Some(TIMEOUT))?;

	probe(stream)
}

fn probe<S: Read + Write>(mut stream: S) -> Result {
	stream.write_all(REQUEST)?;

	let mut head = [0_u8; 12];
	stream.read_exact(&mut head)?;

	if head.starts_with(b"HTTP/1.") && head.ends_with(b" 200") {
		return Ok(());
	}

	let head = String::from_utf8_lossy(&head);

	Err!("Unexpected response from listener: {head:?}")
}

fn loopback(addr: SocketAddr) -> SocketAddr {
	match addr.ip() {
		| IpAddr::V4(ip) if ip.is_unspecified() =>
			SocketAddr::new(Ipv4Addr::LOCALHOST.into(), addr.port()),
		| IpAddr::V6(ip) if ip.is_unspecified() =>
			SocketAddr::new(Ipv6Addr::LOCALHOST.into(), addr.port()),
		| _ => addr,
	}
}

fn probe_tcp(addr: SocketAddr, tls: bool) -> Result {
	let stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;

	// Plaintext probing of a direct TLS listener ends at connection acceptance.
	if tls {
		return Ok(());
	}

	stream.set_read_timeout(Some(TIMEOUT))?;
	stream.set_write_timeout(Some(TIMEOUT))?;

	probe(stream)
}
