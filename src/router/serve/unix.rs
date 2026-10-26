#![cfg(unix)]

use std::{
	fs,
	net::{IpAddr, Ipv4Addr, SocketAddr},
	os::unix::{self, fs::PermissionsExt, net::UnixListener},
	path::Path,
};

use axum::{Extension, Router, extract::ConnectInfo};
use axum_server::Handle;
use futures::{FutureExt, future::BoxFuture};
use tuwunel_core::{Result, err, warn};

#[tracing::instrument(skip_all, level = "debug")]
pub(super) async fn serve<'a>(
	router: &Router,
	handle: &Handle<unix::net::SocketAddr>,
	listeners: impl Iterator<Item = UnixListener>,
	path: Option<&Path>,
	socket_perms: u32,
) -> Result<Vec<BoxFuture<'a, Result<(), std::io::Error>>>> {
	// Loopback so a unix-socket peer bypasses a configured `http.ip_source`.
	let router = router
		.clone()
		.layer(Extension(ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))))
		.into_make_service();

	let mut acceptors = listeners
		.map(|listener| {
			Ok(axum_server::from_unix(listener)?
				.handle(handle.clone())
				.serve(router.clone())
				.boxed())
		})
		.collect::<Result<Vec<_>>>()?;

	if let Some(path) = path {
		if path.exists() {
			warn!("Removing existing UNIX socket {path:?} (unclean shutdown?)...");
			fs::remove_file(path).map_err(|e| {
				err!(Config(
					"http.listen.unix_socket_path",
					"Failed to remove stale UNIX socket at {path:?}: {e}",
				))
			})?;
		}

		let unix_listener = UnixListener::bind(path).map_err(|e| {
			err!(Config(
				"http.listen.unix_socket_path",
				"Failed to bind UNIX socket at {path:?}: {e}",
			))
		})?;

		unix_listener.set_nonblocking(true)?;

		let perms = fs::Permissions::from_mode(socket_perms);
		fs::set_permissions(path, perms).map_err(|e| {
			err!(Config(
				"http.listen.unix_socket_path",
				"Failed to set permissions {socket_perms:o} on UNIX socket at {path:?}: {e}",
			))
		})?;

		let bound_acceptor = axum_server::from_unix(unix_listener)?
			.handle(handle.clone())
			.serve(router)
			.inspect({
				let path = path.to_owned();
				|_| {
					if let Err(err) = fs::remove_file(path) {
						warn!("Failed to remove UNIX socket: {err}");
					}
				}
			})
			.boxed();

		acceptors.push(bound_acceptor);
	}

	Ok(acceptors)
}
