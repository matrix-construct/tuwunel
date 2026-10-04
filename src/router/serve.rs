mod plain;
#[cfg(test)]
mod tests;
#[cfg(feature = "direct_tls")]
mod tls;
mod unix;

#[cfg(unix)]
use std::{borrow::Cow, os::unix::net::UnixListener};
use std::{
	iter::once,
	net::{Ipv4Addr, SocketAddr, TcpListener},
	path::Path,
	sync::{Arc, atomic::Ordering},
};

use tokio::task::JoinSet;
use tuwunel_core::{Err, Result, debug_info, error, info};
#[cfg(unix)]
use tuwunel_core::{itertools::Itertools, utils::sys::is_ipv6_only};
use tuwunel_service::Services;

use super::layers;
use crate::handle::ServerHandle;

/// Serve clients
pub(super) async fn serve(services: Arc<Services>, handle: ServerHandle) -> Result {
	let server = &services.server;
	let config = &server.config;

	#[cfg(not(feature = "oidc"))]
	if services.oauth.get_server().is_ok() {
		return if config.oidc_native_auth {
			Err!(Config("oidc_native_auth", "tuwunel was not built with OIDC support (\"oidc\")"))
		} else {
			Err!(Config(
				"identity_provider",
				"tuwunel was not built with OIDC support (\"oidc\")"
			))
		};
	}

	#[cfg(not(feature = "mas"))]
	if config
		.mas_secret
		.as_deref()
		.is_some_and(|secret| !secret.is_empty())
	{
		return Err!(Config(
			"mas_secret",
			"tuwunel was not built with MAS provisioning support (\"mas\")"
		));
	}

	let (app, _guard) = layers::build(&services)?;

	let mut join_set = JoinSet::new();

	let socket_path = &config.unix_socket_path;

	#[cfg(unix)]
	let (passed_tcp_listeners, passed_unix_listeners) = systemd_listeners()?;
	#[cfg(not(unix))]
	let passed_tcp_listeners: Vec<TcpListener> = Vec::new();

	let addrs = config.get_bind_addrs();

	#[cfg(unix)]
	let mut listening = passed_addrs(&passed_tcp_listeners)?;
	#[cfg(not(unix))]
	let mut listening: Vec<SocketAddr> = Vec::new();

	#[cfg(unix)]
	let socket_path = unpassed_path(socket_path.as_deref(), &passed_unix_listeners)?;
	#[cfg(not(unix))]
	let socket_path = socket_path.as_deref();

	let defaulted = config.is_address_defaulted();
	let tcp_listeners = bind_addrs(&addrs, &mut listening, defaulted)?;

	#[cfg(unix)]
	let log_addrs = make_log_addrs(
		&tcp_listeners,
		socket_path,
		&passed_tcp_listeners,
		&passed_unix_listeners,
	)?;
	#[cfg(not(unix))]
	let log_addrs = make_log_addrs(&tcp_listeners, socket_path, &passed_tcp_listeners)?;

	let mut futures = vec![];

	#[cfg(unix)]
	{
		let socket_perms = config.get_unix_socket_perms()?;

		let unix_futures = unix::serve(
			&app,
			&handle.handle_unix,
			passed_unix_listeners.into_iter(),
			socket_path,
			socket_perms,
		)
		.await?;

		futures.extend(unix_futures);
	};

	#[cfg_attr(
		not(feature = "direct_tls"),
		expect(clippy::redundant_else, unused_variables)
	)]
	if let Some((cert, key)) = config.tls.get_tls_cert_key() {
		#[cfg(feature = "direct_tls")]
		{
			services.globals.init_rustls_provider()?;

			let listeners = tcp_listeners
				.into_iter()
				.chain(passed_tcp_listeners);

			let tls_futures = tls::serve(
				&app,
				&handle.handle_ip,
				cert,
				key,
				config.tls.dual_protocol,
				listeners,
			)
			.await?;

			futures.extend(tls_futures);
		}

		#[cfg(not(feature = "direct_tls"))]
		return tuwunel_core::Err!(Config(
			"tls",
			"tuwunel was not built with direct TLS support (\"direct_tls\")"
		));
	} else {
		let listeners = tcp_listeners
			.into_iter()
			.chain(passed_tcp_listeners);

		let plain_futures = plain::serve(&app, &handle.handle_ip, listeners)?;

		futures.extend(plain_futures);
	}

	for future in futures {
		join_set.spawn_on(future, server.runtime());
	}

	if join_set.is_empty() {
		return Err!("at least one listener should be installed");
	}

	info!("Listening on {log_addrs:?}");

	join_set
		.join_all()
		.await
		.into_iter()
		.filter_map(Result::err)
		.for_each(|e| error!("Listener stopped: {e}"));

	let handle_active = server
		.metrics
		.requests_handle_active
		.load(Ordering::Acquire);

	debug_info!(
		handle_finished = server
			.metrics
			.requests_handle_finished
			.load(Ordering::Acquire),
		panics = server
			.metrics
			.requests_panic
			.load(Ordering::Acquire),
		handle_active,
		"Stopped listening on {log_addrs:?}",
	);

	debug_assert_eq!(0, handle_active, "active request handles still pending");

	Ok(())
}

/// Addresses the service manager already listens on for us.
#[cfg(unix)]
fn passed_addrs(listeners: &[TcpListener]) -> Result<Vec<SocketAddr>> {
	listeners
		.iter()
		.map(listening_addrs)
		.flatten_ok()
		.try_collect()
}

/// Addresses a listener answers on. An unspecified address stands for every
/// address of its family, and a dual-stack listener stands for the IPv4
/// wildcard as well.
fn listening_addrs(listener: &TcpListener) -> Result<impl Iterator<Item = SocketAddr> + use<>> {
	let addr = listener.local_addr()?;

	#[cfg(unix)]
	let dual_stack = addr.is_ipv6() && addr.ip().is_unspecified() && !is_ipv6_only(listener)?;
	#[cfg(not(unix))]
	let dual_stack = false;

	let wildcard = dual_stack.then(|| SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), addr.port()));

	Ok(wildcard.into_iter().chain(once(addr)))
}

/// Whether a listener on one address answers for another, which is the case
/// when they name the same socket or the listener holds the wider wildcard.
fn covers(listening: &SocketAddr, addr: &SocketAddr) -> bool {
	listening.port() == addr.port()
		&& (listening.ip() == addr.ip()
			|| (listening.ip().is_unspecified() && listening.is_ipv6() == addr.is_ipv6()))
}

/// Drops the configured unix socket when a passed listener already binds that
/// path, which would otherwise be unlinked and replaced on the way up.
#[cfg(unix)]
fn unpassed_path<'a>(
	path: Option<&'a Path>,
	listeners: &[UnixListener],
) -> Result<Option<&'a Path>> {
	let Some(configured) = path else {
		return Ok(None);
	};

	let covered = listeners
		.iter()
		.map(UnixListener::local_addr)
		.process_results(|mut addrs| {
			addrs.any(|addr| {
				addr.as_pathname()
					.is_some_and(|passed| same_path(configured, passed))
			})
		})?;

	if covered {
		info!(?configured, "Not binding: the service manager passed a listener for it.");
	}

	Ok(path.filter(|_| !covered))
}

/// Paths naming one socket, however the configuration spelled it.
#[cfg(unix)]
fn same_path(configured: &Path, passed: &Path) -> bool {
	configured == passed
		|| matches!(
			(configured.canonicalize(), passed.canonicalize()),
			(Ok(configured), Ok(passed)) if configured == passed
		)
}

/// Binds the addresses nothing answers for yet, extending `listening` as it
/// goes so a wildcard entry covers the ones behind it.
///
/// Failures surface here, before any listener is served. A defaulted address
/// is skipped instead, since the built-in pair only guesses which loopback
/// families the host has.
fn bind_addrs(
	addrs: &[SocketAddr],
	listening: &mut Vec<SocketAddr>,
	defaulted: bool,
) -> Result<Vec<TcpListener>> {
	let mut listeners = Vec::with_capacity(addrs.len());

	// A dual-stack IPv6 socket answers for the IPv4 wildcard too, and the kernel
	// refuses it once that wildcard is bound, so IPv6 goes first whatever order
	// the configuration lists.
	let mut addrs = addrs.to_vec();
	addrs.sort_by_key(SocketAddr::is_ipv4);

	for addr in &addrs {
		if listening.iter().any(|other| covers(other, addr)) {
			info!(%addr, "Not binding: a listener already answers for it.");
			continue;
		}

		let listener = match TcpListener::bind(addr) {
			| Ok(listener) => listener,
			| Err(e) if defaulted => {
				error!(%addr, %e, "Not binding: the default address is unavailable.");
				continue;
			},
			| Err(e) => return Err!(Config("address", "Failed to bind {addr}: {e}")),
		};

		listener.set_nonblocking(true)?;
		listening.extend(listening_addrs(&listener)?);
		listeners.push(listener);
	}

	Ok(listeners)
}

#[cfg(unix)]
fn make_log_addrs(
	tcp_listeners: &[TcpListener],
	unix_path: Option<&Path>,
	passed_tcp_listeners: &[TcpListener],
	passed_unix_listeners: &[UnixListener],
) -> Result<Vec<String>> {
	let tcp_log_addrs = tcp_listeners
		.iter()
		.map(|listener| Ok(format!("tcp:{}", listener.local_addr()?)));

	let unix_log_addr = unix_path.as_ref().map(|socket_path| {
		let path = socket_path.to_string_lossy();

		format!("unix:{path}")
	});

	let passed_tcp_log_addrs = passed_tcp_listeners.iter().map(|listener| {
		let addr = listener.local_addr()?;

		Ok(format!("passed:tcp:{addr}"))
	});

	let passed_unix_log_addrs = passed_unix_listeners.iter().map(|listener| {
		let addr = listener.local_addr()?;
		let log_path = addr
			.as_pathname()
			.map_or(Cow::Borrowed("?"), Path::to_string_lossy);

		Ok(format!("passed:unix:{log_path}"))
	});

	tcp_log_addrs
		.chain(unix_log_addr.into_iter().map(Ok))
		.chain(passed_tcp_log_addrs)
		.chain(passed_unix_log_addrs)
		.collect()
}

#[cfg(all(unix, feature = "systemd", target_os = "linux"))]
fn systemd_listeners() -> Result<(Vec<TcpListener>, Vec<UnixListener>)> {
	use std::os::fd::FromRawFd;

	use tuwunel_core::utils::sys::{SocketFamily, get_socket_family};

	let mut tcp = vec![];
	let mut unix = vec![];

	for fd in sd_notify::listen_fds()? {
		debug_assert!(fd >= 3, "fdno probably not a listener socket");

		let family = get_socket_family(fd)?;

		match family {
			| SocketFamily::Inet => {
				// SAFETY: systemd should already take care of providing
				// the correct TCP socket, so we just use it via raw fd
				let listener = unsafe { TcpListener::from_raw_fd(fd) };

				listener.set_nonblocking(true)?;

				tcp.push(listener);
			},
			| SocketFamily::Unix => {
				// SAFETY: systemd should already take care of providing
				// the correct UNIX socket, so we just use it via raw fd
				let listener = unsafe { UnixListener::from_raw_fd(fd) };

				listener.set_nonblocking(true)?;

				unix.push(listener);
			},
		}
	}

	Ok((tcp, unix))
}

#[cfg(all(
	unix,
	any(not(feature = "systemd"), not(target_os = "linux"))
))]
fn systemd_listeners() -> Result<(Vec<TcpListener>, Vec<UnixListener>)> { Ok((vec![], vec![])) }

#[cfg(not(unix))]
fn make_log_addrs(
	tcp_listeners: &[TcpListener],
	unix_path: Option<&Path>,
	passed_tcp_listeners: &[TcpListener],
) -> Result<Vec<String>> {
	let tcp_log_addrs = tcp_listeners
		.iter()
		.map(|listener| Ok(format!("tcp:{}", listener.local_addr()?)));

	let unix_log_addr = unix_path.as_ref().map(|socket_path| {
		let path = socket_path.to_string_lossy();
		format!("unix:{path}")
	});

	let passed_tcp_log_addrs = passed_tcp_listeners.iter().map(|listener| {
		let addr = listener.local_addr()?;
		Ok(format!("passed:tcp:{addr}"))
	});

	tcp_log_addrs
		.chain(unix_log_addr.into_iter().map(Ok))
		.chain(passed_tcp_log_addrs)
		.collect()
}
