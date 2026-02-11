#![cfg(unix)]

use std::{
	fs,
	net::SocketAddr,
	os::unix::{fs::PermissionsExt, net::UnixListener},
	path::Path,
	sync::Arc,
};

use axum::{Extension, Router, extract::ConnectInfo};
use axum_server::Handle;
use futures::FutureExt;
use tokio::task::JoinSet;
use tuwunel_core::{Result, Server, info, warn};

#[tracing::instrument(skip_all, level = "debug")]
pub(super) async fn serve(
	server: &Arc<Server>,
	router: &Router,
	handle: &Handle<std::os::unix::net::SocketAddr>,
	join_set: &mut JoinSet<core::result::Result<(), std::io::Error>>,
	path: &Path,
	socket_perms: u32,
) -> Result {
	if path.exists() {
		warn!("Removing existing UNIX socket {path:?} (unclean shutdown?)...");
		fs::remove_file(path)?;
	}

	let unix_listener = UnixListener::bind(path)?;
	unix_listener.set_nonblocking(true)?;

	let perms = fs::Permissions::from_mode(socket_perms);
	fs::set_permissions(path, perms)?;

	let router = router
		.clone()
		.layer(Extension(ConnectInfo("0.0.0.0".parse::<SocketAddr>())))
		.into_make_service();
	let acceptor = axum_server::from_unix(unix_listener)?
		.handle(handle.clone())
		.serve(router)
		.map({
			let path = path.to_owned();
			|_| fs::remove_file(path)
		});
	join_set.spawn_on(acceptor, server.runtime());

	info!("Listening at {path:?}");

	Ok(())
}
