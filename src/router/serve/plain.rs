use std::{
	net::{SocketAddr, TcpListener},
	sync::Arc,
};

use axum::Router;
use axum_server::Handle;
use tokio::task::JoinSet;
use tuwunel_core::{Result, Server, info};

pub(super) fn serve(
	server: &Arc<Server>,
	router: &Router,
	handle: &Handle<SocketAddr>,
	join_set: &mut JoinSet<Result<(), std::io::Error>>,
	listeners: &[TcpListener],
	addrs: &[SocketAddr],
) -> Result {
	let router = router
		.clone()
		.into_make_service_with_connect_info::<SocketAddr>();

	for listener in listeners {
		let acceptor = axum_server::from_tcp(listener.try_clone()?)?
			.handle(handle.clone())
			.serve(router.clone());

		join_set.spawn_on(acceptor, server.runtime());
	}

	info!("Listening on {addrs:?}");

	Ok(())
}
