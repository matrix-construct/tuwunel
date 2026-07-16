use std::{
	sync::{Arc, Weak, atomic::Ordering},
	time::Duration,
};

use futures::{FutureExt, future::join, pin_mut};
#[cfg(all(feature = "systemd", target_os = "linux"))]
use sd_notify::{NotifyState, notify, notify_and_unset_env, watchdog_enabled};
use tuwunel_core::{
	Error, Result, Server, debug, debug_error, debug_info, error, info, utils::BoolExt,
};
use tuwunel_service::Services;

use crate::{handle::ServerHandle, serve};

/// Main loop base
#[tracing::instrument(skip_all)]
pub(crate) async fn run(services: Arc<Services>) -> Result {
	let server = &services.server;
	debug!("Start");

	// Install the admin command root here for now
	tuwunel_admin::init(&services.admin);

	// Execute configured startup commands.
	services.admin.startup_execute().await?;

	// Setup shutdown/signal handling
	let handle = ServerHandle::new();
	let sigs = server
		.runtime()
		.spawn(signal(server.clone(), handle.clone()));
	#[cfg(all(feature = "systemd", target_os = "linux"))]
	let watchdog = server.runtime().spawn(start_systemd_watchdog());

	let non_listener = services
		.config
		.listening
		.is_false()
		.then_async(|| server.until_shutdown().map(Ok));

	let listener = services.config.listening.then_async(|| {
		server
			.runtime()
			.spawn(serve::serve(services.clone(), handle))
			.map(|res| res.map_err(Error::from).unwrap_or_else(Err))
	});

	// Focal point
	debug!("Running");
	pin_mut!(listener, non_listener);
	let res = tokio::select! {
		res = join(&mut listener, &mut non_listener) => {
			res.0.unwrap_or(res.1.unwrap_or(Ok(())))
		},
		res = services.poll() => {
			server.until_shutdown().await;
			handle_services_finish(server, res, listener.await)
		},
	};

	// Join watchdog and the signal handler before we leave.
	#[cfg(all(feature = "systemd", target_os = "linux"))]
	{
		watchdog.abort();
		_ = watchdog.await;
	};

	sigs.abort();
	_ = sigs.await;

	// Remove the admin command root
	tuwunel_admin::fini(&services.admin);

	debug_info!("Finish");
	res
}

/// Async initializations
#[tracing::instrument(skip_all)]
pub(crate) async fn start(server: Arc<Server>) -> Result<Arc<Services>> {
	debug!("Starting...");

	#[cfg(all(feature = "systemd", target_os = "linux"))]
	let keepalive = server.runtime().spawn(extend_systemd_startup());

	let services = async move { Services::build(server).await?.start().await }.await;

	#[cfg(all(feature = "systemd", target_os = "linux"))]
	{
		keepalive.abort();
		_ = keepalive.await;
	};

	let services = services?;

	#[cfg(all(feature = "systemd", target_os = "linux"))]
	notify(&[NotifyState::Ready]).expect("failed to notify systemd of ready state");

	debug!("Started");
	Ok(services)
}

/// Async destructions
#[tracing::instrument(skip_all)]
pub(crate) async fn stop(services: Arc<Services>) -> Result {
	debug!("Shutting down...");

	#[cfg(all(feature = "systemd", target_os = "linux"))]
	notify_systemd_shutdown(&services.server);

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

#[cfg(all(feature = "systemd", target_os = "linux"))]
fn notify_systemd_shutdown(server: &Server) {
	// An in-place exec restart keeps this PID; report a reload, not an exit, so
	// the unit stays active and NOTIFY_SOCKET survives for the next image. The
	// watchdog stays armed while reloading, so reset it to give teardown and
	// exec the full interval.
	if server.is_restarting() {
		let monotonic = NotifyState::monotonic_usec_now().expect("failed to get monotonic time");

		notify(&[NotifyState::Reloading, monotonic, NotifyState::Watchdog])
			.expect("failed to notify systemd of reloading state");

		return;
	}

	// SAFETY: clears NOTIFY_SOCKET from the process environment. Safe because no
	// other thread reads or writes that variable; this matches the previous
	// `notify(unset_env=true, ...)` semantics from sd-notify 0.4.
	unsafe { notify_and_unset_env(&[NotifyState::Stopping]) }
		.expect("failed to notify systemd of stopping state");
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

	if server.is_running()
		&& let Err(e) = server.shutdown()
	{
		error!("Failed to send shutdown signal: {e}");
	}

	if let Some(Err(e)) = listener {
		error!("Client listener task finished with error: {e}");
	}

	result
}

#[cfg(all(feature = "systemd", target_os = "linux"))]
async fn start_systemd_watchdog() {
	use tokio::time::MissedTickBehavior;

	let Some(watchdog) = watchdog_enabled() else {
		return;
	};

	let watchdog_usec = u64::try_from(watchdog.as_micros()).unwrap_or(u64::MAX);
	let interval_usec = (watchdog_usec / 2).max(1);
	let interval = Duration::from_micros(interval_usec);

	let mut ticker = tokio::time::interval(interval);
	ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
	loop {
		ticker.tick().await;

		if let Err(e) = notify(&[NotifyState::Watchdog]) {
			error!(%e, "failed to notify systemd watchdog state");
		}
	}
}

#[cfg(all(feature = "systemd", target_os = "linux"))]
async fn extend_systemd_startup() {
	use std::env;

	use tokio::time::MissedTickBehavior;

	const INTERVAL: Duration = Duration::from_secs(15);

	// Keep systemd's start timeout extended while a slow boot such as a database
	// migration runs, so a healthy service is not killed before it signals ready.
	if env::var_os("NOTIFY_SOCKET").is_none() {
		return;
	}

	let extend_usec = u32::try_from(INTERVAL.as_micros())
		.unwrap_or(u32::MAX)
		.saturating_mul(2);

	let mut ticker = tokio::time::interval(INTERVAL);
	ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
	loop {
		ticker.tick().await;

		if let Err(e) = notify(&[NotifyState::ExtendTimeoutUsec(extend_usec)]) {
			error!(%e, "failed to extend systemd startup timeout");
		}
	}
}
