use std::{
	net::{SocketAddr, TcpListener},
	sync::Arc,
};

use axum::Router;
use axum_server::Handle;
use axum_server_dual_protocol::{ServerExt, axum_server::tls_rustls::RustlsConfig};
use tokio::task::JoinSet;
use tuwunel_core::{Result, Server, debug, err, info, warn};

pub(super) async fn serve(
	server: &Arc<Server>,
	app: &Router,
	handle: &Handle<SocketAddr>,
	join_set: &mut JoinSet<core::result::Result<(), std::io::Error>>,
	listeners: &[TcpListener],
	addrs: &[SocketAddr],
) -> Result {
	let tls = &server.config.tls;

	let certs = tls
		.certs
		.as_ref()
		.ok_or_else(|| err!(Config("tls.certs", "Invalid or missing TLS certificates")))?;

	let key = tls
		.key
		.as_ref()
		.ok_or_else(|| err!(Config("tls.key", "Invalid or missingTLS key")))?;

	info!(
		"Note: It is strongly recommended that you use a reverse proxy instead of running \
		 tuwunel directly with TLS."
	);

	debug!(
		"Using direct TLS. Certificate path {certs:?} and certificate private key path {key:?}"
	);

	let conf = RustlsConfig::from_pem_file(certs, key)
		.await
		.map_err(|e| err!(Config("tls", "Failed to load certificates or key: {e}")))?;

	let app = app
		.clone()
		.into_make_service_with_connect_info::<SocketAddr>();

	if tls.dual_protocol {
		for listener in listeners {
			let acceptor = axum_server_dual_protocol::from_tcp_dual_protocol(
				listener.try_clone()?,
				conf.clone(),
			)?
			.set_upgrade(false)
			.handle(handle.clone())
			.serve(app.clone());

			join_set.spawn_on(acceptor, server.runtime());
		}

		warn!(
			"Listening on {addrs:?} with TLS certificate {certs} and supporting plain text \
			 (HTTP) connections too (insecure!)",
		);
	} else {
		for listener in listeners {
			let acceptor = axum_server::from_tcp_rustls(listener.try_clone()?, conf.clone())?
				.handle(handle.clone())
				.serve(app.clone());

			join_set.spawn_on(acceptor, server.runtime());
		}

		info!("Listening on {addrs:?} with TLS certificate {certs}");
	}

	Ok(())
}
