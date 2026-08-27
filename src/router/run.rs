#[cfg(all(feature = "systemd", target_os = "linux"))]
use std::env::var_os;
use std::{
	sync::{Arc, Weak, atomic::Ordering},
	time::Duration,
};

use futures::{FutureExt, future::join, pin_mut};
#[cfg(all(feature = "systemd", target_os = "linux"))]
use sd_notify::{NotifyState, notify, notify_and_unset_env, watchdog_enabled};
use tokio::time::{MissedTickBehavior, interval};
use tuwunel_core::{
	Error, Result, Server, debug, debug_error, debug_info, defer, error, info, utils::BoolExt,
	warn,
};
use tuwunel_service::Services;

use crate::{handle::ServerHandle, serve};

/// How often startup reports its progress and extends the service manager's
/// timeout.
///
/// The extension asks for twice this, so one missed tick does not end the
/// grace the service manager is holding open.
const STARTUP_INTERVAL: Duration = Duration::from_secs(15);

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

	// The ticker holds the stop timeout open too, so ending it any earlier lets a
	// stop request kill a migration mid-write.
	let reporter = server
		.runtime()
		.spawn(report_startup_progress(server.clone()));

	let abort = reporter.abort_handle();

	defer! {{
		abort.abort();
	}}

	let services = async move { Services::build(server).await?.start().await }.await;

	reporter.abort();
	_ = reporter.await;

	let services = services?;

	// The status is set here so it reads as a baseline rather than staying blank
	// until the first reload replaces it.
	#[cfg(all(feature = "systemd", target_os = "linux"))]
	notify(&[NotifyState::Ready, NotifyState::Status("Running")])
		.expect("failed to notify systemd of ready state");

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
#[expect(clippy::infinite_loop)]
async fn start_systemd_watchdog() {
	let Some(watchdog) = watchdog_enabled() else {
		return;
	};

	let watchdog_usec = u64::try_from(watchdog.as_micros()).unwrap_or(u64::MAX);
	let interval_usec = (watchdog_usec / 2).max(1);
	let period = Duration::from_micros(interval_usec);

	let mut ticker = interval(period);
	ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
	loop {
		ticker.tick().await;

		notify_systemd(&[NotifyState::Watchdog], "watchdog");
	}
}

/// Reports the long startup phase in flight and keeps the service manager
/// waiting for it.
///
/// A database migration can run for many minutes with nothing else to show for
/// it, so every tick logs the phase, its position and how long it has been
/// running. The same tick extends systemd's timeout, which covers the stop
/// timeout as well as the start timeout, so ending this task early lets a stop
/// request kill a migration mid-write.
#[expect(clippy::infinite_loop)]
async fn report_startup_progress(server: Arc<Server>) {
	#[cfg(all(feature = "systemd", target_os = "linux"))]
	let notifiable = var_os("NOTIFY_SOCKET").is_some();

	let mut ticker = interval(STARTUP_INTERVAL);
	let mut announced = false;

	ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
	loop {
		ticker.tick().await;

		let progress = server.progress.report();

		#[cfg(all(feature = "systemd", target_os = "linux"))]
		if notifiable {
			notify_systemd_startup(progress.as_deref());
		}

		let Some(progress) = progress else {
			continue;
		};

		if announced {
			info!(%progress, "Database migration in progress");
			continue;
		}

		announced = true;
		warn!(
			%progress,
			"Database migration in progress. A large database can take many minutes. A stop \
			 request is honored between steps and every step that finished is recorded, so the \
			 migration resumes where it left off; killing the process instead can leave the \
			 database mid-write."
		);
	}
}

/// Extends the service manager's timeout by another interval and reports what
/// the server is waiting on.
///
/// The extension applies to whichever timeout is armed, so it holds a stop
/// request off a migration in flight as much as it holds off the start
/// timeout. The caller establishes that a service manager is listening.
#[cfg(all(feature = "systemd", target_os = "linux"))]
fn notify_systemd_startup(status: Option<&str>) {
	let extend_usec = u32::try_from(STARTUP_INTERVAL.as_micros())
		.unwrap_or(u32::MAX)
		.saturating_mul(2);

	notify_systemd(&[NotifyState::ExtendTimeoutUsec(extend_usec)], "startup timeout extension");

	let Some(status) = status else {
		return;
	};

	notify_systemd(&[NotifyState::Status(status)], "startup status");
}

/// Sends notification states to the service manager.
///
/// A notification is advisory, so a failure is logged against the name of what
/// could not be sent and never reaches the caller.
#[cfg(all(feature = "systemd", target_os = "linux"))]
fn notify_systemd(states: &[NotifyState<'_>], about: &'static str) {
	if let Err(e) = notify(states) {
		error!(%e, %about, "failed to notify systemd");
	}
}
