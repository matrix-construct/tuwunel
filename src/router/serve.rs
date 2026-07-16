mod plain;
#[cfg(feature = "direct_tls")]
mod tls;
mod unix;

use std::{
	borrow::Cow,
	net::{SocketAddr, TcpListener},
	os::unix::net::UnixListener,
	path::Path,
	sync::{Arc, atomic::Ordering},
};

use tokio::task::JoinSet;
use tuwunel_core::{Err, Result, debug_info, info};
use tuwunel_service::Services;

use super::layers;
use crate::handle::ServerHandle;

/// Serve clients
pub(super) async fn serve(services: Arc<Services>, handle: ServerHandle) -> Result {
	let server = &services.server;
	let config = &server.config;

	let (app, _guard) = layers::build(&services)?;

	let mut join_set = JoinSet::new();

	let socket_path = &config.unix_socket_path;

	let (passed_tcp_listeners, passed_unix_listeners) = systemd_listeners()?;

	let addrs = config.get_bind_addrs();

	let log_addrs = make_log_addrs(
		&addrs,
		socket_path.as_deref(),
		&passed_tcp_listeners,
		&passed_unix_listeners,
	)?;

	let mut futures = vec![];

	#[cfg(unix)]
	{
		let socket_perms = config.get_unix_socket_perms()?;

		let unix_futures = unix::serve(
			&app,
			&handle.handle_unix,
			passed_unix_listeners.into_iter(),
			socket_path.as_deref(),
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

			let tls_futures = tls::serve(
				&app,
				&handle.handle_ip,
				cert,
				key,
				config.tls.dual_protocol,
				passed_tcp_listeners.into_iter(),
				&addrs,
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
		let plain_futures =
			plain::serve(&app, &handle.handle_ip, passed_tcp_listeners.into_iter(), &addrs)?;

		futures.extend(plain_futures);
	}

	for future in futures {
		join_set.spawn_on(future, server.runtime());
	}

	if join_set.is_empty() {
		return Err!("at least one listener should be installed");
	}

	info!("Listening on {log_addrs:?}");

	join_set.join_all().await;

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
		"Stopped listening on {addrs:?}",
	);

	debug_assert_eq!(0, handle_active, "active request handles still pending");

	Ok(())
}

fn make_log_addrs(
	tcp_addrs: &[SocketAddr],
	unix_path: Option<&Path>,
	tcp_listeners: &[TcpListener],
	unix_listeners: &[UnixListener],
) -> Result<Vec<String>> {
	let tcp_log_addrs = tcp_addrs.iter().map(|addr| format!("tcp:{addr}"));

	let unix_log_addr = unix_path.as_ref().map(|socket_path| {
		let path = socket_path.to_string_lossy();

		format!("unix:{path}")
	});

	let passed_tcp_log_addrs = tcp_listeners.iter().map(|listener| {
		let addr = listener.local_addr()?;

		Ok(format!("passed:tcp:{addr}"))
	});

	let passed_unix_log_addrs = unix_listeners.iter().map(|listener| {
		let addr = listener.local_addr()?;
		let log_path = addr
			.as_pathname()
			.map_or(Cow::Borrowed("?"), Path::to_string_lossy);

		Ok(format!("passed:unix:{log_path}"))
	});

	tcp_log_addrs
		.map(Ok)
		.chain(unix_log_addr.into_iter().map(Ok))
		.chain(passed_tcp_log_addrs)
		.chain(passed_unix_log_addrs)
		.collect()
}

#[cfg(all(feature = "systemd", target_os = "linux"))]
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

#[cfg(any(not(feature = "systemd"), not(target_os = "linux")))]
fn systemd_listeners() -> Result<(Vec<TcpListener>, Vec<UnixListener>)> { Ok((vec![], vec![])) }
