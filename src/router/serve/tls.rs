use std::{
	net::{SocketAddr, TcpListener},
	path::Path,
};

use axum::{Router, extract::connect_info::IntoMakeServiceWithConnectInfo};
use axum_server::{Handle, from_tcp_rustls};
use axum_server_dual_protocol::{
	ServerExt, axum_server::tls_rustls::RustlsConfig, from_tcp_dual_protocol,
};
use futures::{FutureExt, future::BoxFuture};
use tuwunel_core::{Result, debug, err, info};

pub(super) async fn serve<'a>(
	app: &Router,
	handle: &Handle<SocketAddr>,
	cert: &Path,
	key: &Path,
	dual_protocol: bool,
	listeners: impl Iterator<Item = TcpListener>,
) -> Result<Vec<BoxFuture<'a, Result<(), std::io::Error>>>> {
	info!(
		"Note: It is strongly recommended that you use a reverse proxy instead of running \
		 tuwunel directly with TLS."
	);

	debug!(
		"Using direct TLS. Certificate path {cert:?} and certificate private key path {key:?}"
	);

	let conf = RustlsConfig::from_pem_file(cert, key)
		.await
		.map_err(|e| err!(Config("tls", "Failed to load certificates or key: {e}")))?;

	let app = app
		.clone()
		.into_make_service_with_connect_info::<SocketAddr>();

	if dual_protocol {
		serve_dual_protocol(&app, &conf, handle, listeners)
	} else {
		serve_tls(&app, &conf, handle, listeners)
	}
}

fn serve_dual_protocol<'a>(
	app: &IntoMakeServiceWithConnectInfo<Router, SocketAddr>,
	conf: &RustlsConfig,
	handle: &Handle<SocketAddr>,
	listeners: impl Iterator<Item = TcpListener>,
) -> Result<Vec<BoxFuture<'a, Result<(), std::io::Error>>>> {
	listeners
		.map(|listener| {
			// Pinned against an upstream default flip: a plain request is answered,
			// never redirected to the https scheme.
			let acceptor = from_tcp_dual_protocol(listener, conf.clone())?
				.set_upgrade(false)
				.handle(handle.clone())
				.serve(app.clone())
				.boxed(); // heterogeneous Vec<BoxFuture>

			Ok(acceptor)
		})
		.collect()
}

fn serve_tls<'a>(
	app: &IntoMakeServiceWithConnectInfo<Router, SocketAddr>,
	conf: &RustlsConfig,
	handle: &Handle<SocketAddr>,
	listeners: impl Iterator<Item = TcpListener>,
) -> Result<Vec<BoxFuture<'a, Result<(), std::io::Error>>>> {
	listeners
		.map(|listener| {
			let acceptor = from_tcp_rustls(listener, conf.clone())?
				.handle(handle.clone())
				.serve(app.clone())
				.boxed(); // heterogeneous Vec<BoxFuture>

			Ok(acceptor)
		})
		.collect()
}
