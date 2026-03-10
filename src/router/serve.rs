mod plain;
#[cfg(feature = "direct_tls")]
mod tls;
mod unix;

use std::{
	net::{SocketAddr, TcpListener},
	sync::{Arc, atomic::Ordering},
};

use tokio::task::JoinSet;
use tuwunel_core::{Result, debug_info};
use tuwunel_service::Services;

use super::layers;
use crate::handle::ServerHandle;

/// Serve clients
pub(super) async fn serve(services: Arc<Services>, handle: ServerHandle) -> Result {
	let server = &services.server;
	let config = &server.config;

	let (app, _guard) = layers::build(&services)?;

	let mut join_set = JoinSet::new();

	#[cfg(unix)]
	if let Some(unix_socket) = &config.unix_socket_path {
		let socket_perms = config.get_unix_socket_perms()?;

		unix::serve(server, &app, &handle.handle_unix, &mut join_set, unix_socket, socket_perms)
			.await?;
	}

	let systemd_listeners: Vec<_> = systemd_listeners().collect();
	let systemd_listeners_is_empty = systemd_listeners.is_empty();
	let (listeners, addrs): (Vec<_>, Vec<_>) = config
		.get_bind_addrs()
		.into_iter()
		.filter(|_| !systemd_listeners_is_empty)
		.map(|addr| {
			let listener = TcpListener::bind(addr)
				.expect("Failed to bind configured TcpListener to {addr:?}");

			(listener, addr)
		})
		.chain(systemd_listeners)
		.inspect(|(listener, _)| {
			listener
				.set_nonblocking(true)
				.expect("Failed to set non-blocking");
		})
		.unzip();

	#[cfg_attr(not(feature = "direct_tls"), expect(clippy::redundant_else))]
	if config.tls.certs.is_some() {
		#[cfg(feature = "direct_tls")]
		{
			services.globals.init_rustls_provider()?;
			tls::serve(server, &app, &handle.handle_ip, &mut join_set, &listeners, &addrs)
				.await?;
		}

		#[cfg(not(feature = "direct_tls"))]
		return tuwunel_core::Err!(Config(
			"tls",
			"tuwunel was not built with direct TLS support (\"direct_tls\")"
		));
	} else {
		plain::serve(server, &app, &handle.handle_ip, &mut join_set, &listeners, &addrs)?;
	}

	assert!(!join_set.is_empty(), "at least one listener should be installed");

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

#[cfg(all(feature = "systemd", target_os = "linux"))]
fn systemd_listeners() -> impl Iterator<Item = (TcpListener, SocketAddr)> {
	sd_notify::listen_fds()
		.into_iter()
		.flatten()
		.filter_map(|fd| {
			use std::os::fd::FromRawFd;

			debug_assert!(fd >= 3, "fdno probably not a listener socket");
			// SAFETY: systemd should already take care of providing
			// the correct TCP socket, so we just use it via raw fd
			let listener = unsafe { TcpListener::from_raw_fd(fd) };

			let Ok(addr) = listener.local_addr() else {
				return None;
			};

			Some((listener, addr))
		})
}

#[cfg(any(not(feature = "systemd"), not(target_os = "linux")))]
fn systemd_listeners() -> impl Iterator<Item = (TcpListener, SocketAddr)> { std::iter::empty() }
