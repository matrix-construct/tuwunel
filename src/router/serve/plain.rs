use std::net::{SocketAddr, TcpListener};

use axum::Router;
use axum_server::{Handle, from_tcp};
use futures::{FutureExt, future::BoxFuture};
use tuwunel_core::Result;

pub(super) fn serve<'a>(
	router: &Router,
	handle: &Handle<SocketAddr>,
	listeners: impl Iterator<Item = TcpListener>,
) -> Result<Vec<BoxFuture<'a, Result<(), std::io::Error>>>> {
	let router = router
		.clone()
		.into_make_service_with_connect_info::<SocketAddr>();

	listeners
		.map(|listener| {
			let acceptor = from_tcp(listener)?
				.handle(handle.clone())
				.serve(router.clone())
				.boxed(); // heterogeneous Vec<BoxFuture>

			Ok(acceptor)
		})
		.collect()
}
