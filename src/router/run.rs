use std::{
	sync::{Arc, Weak, atomic::Ordering},
	time::Duration,
};

use futures::{FutureExt, pin_mut};
use tuwunel_core::{
	Error, Result, Server, debug, debug_error, debug_info, error, info,
	utils::{BoolExt, future::OptionFutureExt},
};
use tuwunel_service::Services;

use crate::{handle::ServerHandle, serve};

/// Main loop base
#[tracing::instrument(skip_all)]
pub(crate) async fn run(services: Arc<Services>) -> Result {
	let server = &services.server;
	debug!("Start");

	// Install the admin room callback here for now
	tuwunel_admin::init(&services.admin).await;

	// Setup shutdown/signal handling
	let handle = ServerHandle::new();
	let sigs = server
		.runtime()
		.spawn(signal(server.clone(), handle.clone()));

	let listener = services
		.config
		.listening
		.then_async(|| {
			server
				.runtime()
				.spawn(serve::serve(services.clone(), handle))
				.map(|res| res.map_err(Error::from).unwrap_or_else(Err))
		})
		.unwrap_or_else_async(|| server.until_shutdown().map(Ok));

	// Focal point
	debug!("Running");
	pin_mut!(listener);
	let res = tokio::select! {
		res = &mut listener => res.unwrap_or(Ok(())),
		res = services.poll() => handle_services_finish(server, res, listener.await),
	};

	// Join the signal handler before we leave.
	sigs.abort();
	_ = sigs.await;

	// Remove the admin room callback
	tuwunel_admin::fini(&services.admin).await;

	debug_info!("Finish");
	res
}

/// Async initializations
#[tracing::instrument(skip_all)]
pub(crate) async fn start(server: Arc<Server>) -> Result<Arc<Services>> {
	debug!("Starting...");

	let services = Services::build(server).await?.start().await?;

	#[cfg(all(feature = "systemd", target_os = "linux"))]
	sd_notify::notify(true, &[sd_notify::NotifyState::Ready])
		.expect("failed to notify systemd of ready state");

	debug!("Started");
	Ok(services)
}

/// Async destructions
#[tracing::instrument(skip_all)]
pub(crate) async fn stop(services: Arc<Services>) -> Result {
	debug!("Shutting down...");

	#[cfg(all(feature = "systemd", target_os = "linux"))]
	sd_notify::notify(true, &[sd_notify::NotifyState::Stopping])
		.expect("failed to notify systemd of stopping state");

	// Wait for all completions before dropping or we'll lose them to the module
	// unload and explode.
	services.stop().await;

	// Check that Services and Database will drop as expected, The complex of Arc's
	// used for various components can easily lead to references being held
	// somewhere improperly; this can hang shutdowns.
	debug!("Cleaning up...");
	let db = Arc::downgrade(&services.db);
	if let Err(services) = Arc::try_unwrap(services) {
		debug_error!(
			"{} dangling references to Services after shutdown",
			Arc::strong_count(&services)
		);
	}

	if Weak::strong_count(&db) > 0 {
		debug_error!(
			"{} dangling references to Database after shutdown",
			Weak::strong_count(&db)
		);
	}

	info!("Shutdown complete.");
	Ok(())
}

#[tracing::instrument(skip_all)]
async fn signal(server: Arc<Server>, handle: ServerHandle) {
	server.until_shutdown().await;
	handle_shutdown(&server, &handle);
}

fn handle_shutdown(server: &Arc<Server>, handle: &ServerHandle) {
	let timeout = server.config.client_shutdown_timeout;
	let timeout = Duration::from_secs(timeout);
	debug!(
		?timeout,
		handle_active = ?server.metrics.requests_handle_active.load(Ordering::Relaxed),
		"Notifying for graceful shutdown"
	);

	handle.graceful_shutdown(Some(timeout));
}

fn handle_services_finish(
	server: &Arc<Server>,
	result: Result,
	listener: Option<Result>,
) -> Result {
	debug!("Service manager finished: {result:?}");

	if server.running()
		&& let Err(e) = server.shutdown()
	{
		error!("Failed to send shutdown signal: {e}");
	}

	if let Some(Err(e)) = listener {
		error!("Client listener task finished with error: {e}");
	}

	result
}
