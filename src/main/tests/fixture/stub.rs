//! A TLS listener for the stub federation peers some boot tests run.
//!
//! It stands apart from the boot fixture so that only a binary including it
//! compiles it. The certificate is Complement's self-signed one on a loopback
//! address, so the server under test reaches a stub only with
//! `network.http.allow_invalid_tls_certificates` on and loopback off its
//! `network.http.ip_range_denylist`.

use std::{io, net::TcpListener};

use axum::Router;
use axum_server::{from_tcp_rustls, tls_rustls::RustlsConfig};
use tokio::{runtime::Handle, task::JoinSet};
use tuwunel_core::Result;
use tuwunel_service::Services;

/// Owns the task serving one federation stub.
///
/// Dropping the set aborts the listener task.
pub(crate) type Stub = JoinSet<io::Result<()>>;

const CERTIFICATE: &str =
	concat!(env!("CARGO_MANIFEST_DIR"), "/../../nix/pkgs/complement/certificate.crt");

const PRIVATE_KEY: &str =
	concat!(env!("CARGO_MANIFEST_DIR"), "/../../nix/pkgs/complement/private_key.key");

/// Serves the app over TLS on the listener, from the server's runtime.
///
/// The stub answers until the caller drops the returned set.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn serve(
	services: &Services,
	listener: TcpListener,
	app: Router,
) -> Result<Stub> {
	let config = RustlsConfig::from_pem_file(CERTIFICATE, PRIVATE_KEY).await?;

	listener.set_nonblocking(true)?;

	let remote = from_tcp_rustls(listener, config)?.serve(app.into_make_service());

	Ok(spawned(remote, services.server.runtime()))
}

fn spawned<Remote>(remote: Remote, runtime: &Handle) -> Stub
where
	Remote: Future<Output = io::Result<()>> + Send + 'static,
{
	let mut stub = Stub::new();

	stub.spawn_on(remote, runtime);
	stub
}
