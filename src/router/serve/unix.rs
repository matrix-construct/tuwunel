#![cfg(unix)]

use std::{
	net::{self, IpAddr, Ipv4Addr},
	os::fd::AsRawFd,
	path::Path,
	sync::{Arc, atomic::Ordering},
};

use axum::{
	Router,
	extract::{Request, connect_info::IntoMakeServiceWithConnectInfo},
};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::{
	rt::{TokioExecutor, TokioIo},
	server,
};
use tokio::{
	fs,
	net::{UnixListener, UnixStream, unix::SocketAddr},
	sync::broadcast::{self},
	task::JoinSet,
	time::{Duration, sleep},
};
use tower::{Service, ServiceExt};
use tuwunel_core::{
	Err, Result, Server, debug, debug_error, info, result::UnwrapInfallible, trace, warn,
};

type MakeService = IntoMakeServiceWithConnectInfo<Router, net::SocketAddr>;

const NULL_ADDR: net::SocketAddr = net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
const FINI_POLL_INTERVAL: Duration = Duration::from_millis(750);

#[tracing::instrument(skip_all, level = "debug")]
pub(super) async fn serve(
	server: &Arc<Server>,
	app: Router,
	mut shutdown: broadcast::Receiver<()>,
) -> Result {
	let mut tasks = JoinSet::<()>::new();
	let executor = TokioExecutor::new();
	let app = app.into_make_service_with_connect_info::<net::SocketAddr>();
	let builder = server::conn::auto::Builder::new(executor);
	let listener = init(server).await?;
	while server.running() {
		let app = app.clone();
		let builder = builder.clone();
		tokio::select! {
			_sig = shutdown.recv() => break,
			conn = listener.accept() => match conn {
				Ok(conn) => accept(server, &listener, &mut tasks, app, builder, conn).await,
				Err(err) => debug_error!(?listener, "accept error: {err}"),
			},
		}
	}

	fini(server, listener, tasks).await;

	Ok(())
}

#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(
		?listener,
		socket = ?conn.0,
	),
)]
async fn accept(
	server: &Arc<Server>,
	listener: &UnixListener,
	tasks: &mut JoinSet<()>,
	app: MakeService,
	builder: server::conn::auto::Builder<TokioExecutor>,
	conn: (UnixStream, SocketAddr),
) {
	let (socket, _) = conn;
	let server_ = server.clone();
	let task = async move { accepted(server_, builder, socket, app).await };

	_ = tasks.spawn_on(task, server.runtime());
	while tasks.try_join_next().is_some() {}
}

#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(
		fd = %socket.as_raw_fd(),
		path = ?socket.local_addr(),
	),
)]
async fn accepted(
	server: Arc<Server>,
	builder: server::conn::auto::Builder<TokioExecutor>,
	socket: UnixStream,
	mut app: MakeService,
) {
	let socket = TokioIo::new(socket);
	let called = app.call(NULL_ADDR).await.unwrap_infallible();
	let service = move |req: Request<Incoming>| called.clone().oneshot(req);
	let handler = service_fn(service);
	trace!(?socket, ?handler, "serving connection");

	// bug on darwin causes all results to be errors. do not unwrap this
	tokio::select! {
		() = server.until_shutdown() => (),
		_ = builder.serve_connection(socket, handler) => (),
	};
}

async fn init(server: &Arc<Server>) -> Result<UnixListener> {
	use std::os::unix::fs::PermissionsExt;

	let config = &server.config;
	let path = config
		.unix_socket_path
		.as_ref()
		.expect("failed to extract configured unix socket path");

	if path.exists() {
		warn!("Removing existing UNIX socket {:#?} (unclean shutdown?)...", path.display());
		fs::remove_file(&path)
			.await
			.map_err(|e| warn!("Failed to remove existing UNIX socket: {e}"))
			.unwrap();
	}

	let dir = path.parent().unwrap_or_else(|| Path::new("/"));
	if let Err(e) = fs::create_dir_all(dir).await {
		return Err!("Failed to create {dir:?} for socket {path:?}: {e}");
	}

	let listener = UnixListener::bind(path);
	if let Err(e) = listener {
		return Err!("Failed to bind listener {path:?}: {e}");
	}

	let socket_perms = config.unix_socket_perms.to_string();
	let octal_perms =
		u32::from_str_radix(&socket_perms, 8).expect("failed to convert octal permissions");
	let perms = std::fs::Permissions::from_mode(octal_perms);
	if let Err(e) = fs::set_permissions(&path, perms).await {
		return Err!("Failed to set socket {path:?} permissions: {e}");
	}

	info!("Listening at {path:?}");

	Ok(listener.unwrap())
}

async fn fini(server: &Arc<Server>, listener: UnixListener, mut tasks: JoinSet<()>) {
	let local = listener.local_addr();

	debug!("Closing listener at {local:?} ...");
	drop(listener);

	debug!("Waiting for requests to finish...");
	while server
		.metrics
		.requests_handle_active
		.load(Ordering::Acquire)
		.gt(&0)
	{
		tokio::select! {
			task = tasks.join_next() => if task.is_none() { break; },
			() = sleep(FINI_POLL_INTERVAL) => {},
		}
	}

	debug!("Shutting down...");
	tasks.shutdown().await;

	if let Ok(local) = local
		&& let Some(path) = local.as_pathname()
	{
		debug!(?path, "Removing unix socket file.");
		if let Err(e) = fs::remove_file(path).await {
			warn!(?path, "Failed to remove UNIX socket file: {e}");
		}
	}
}
