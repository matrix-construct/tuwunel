pub mod args;
pub mod config;
pub mod health;
pub mod logging;
pub mod mods;
pub mod restart;
pub mod runtime;
pub mod sentry;
pub mod server;
pub mod signals;

use std::sync::Arc;

use log as _;
use tuwunel_core::{
	Error, Result, Server as CoreServer, debug_info, defer, error, info, mod_ctor, mod_dtor,
	rustc_flags_capture,
};
use tuwunel_service::Services;

pub use self::{args::Args, runtime::Runtime, server::Server};

mod_ctor! {}
mod_dtor! {}
rustc_flags_capture! {}

pub fn exec(server: &Arc<Server>, runtime: Runtime) -> Result {
	run(server, &runtime)?;
	drop(runtime);

	Ok(())
}

pub fn run(server: &Arc<Server>, runtime: &Runtime) -> Result {
	runtime.block_on(async_exec(server))
}

/// Operate the server normally in release-mode static builds. This will start,
/// run and stop the server within the asynchronous runtime.
#[tracing::instrument(
    name = "main",
    parent = None,
    skip_all
)]
pub async fn async_exec(server: &Arc<Server>) -> Result {
	let signals = server
		.server
		.runtime()
		.spawn(signals::enable(server.clone()));

	let abort = signals.abort_handle();
	defer! {{
		abort.abort();
	}}

	let started = async_start(server).await;

	// Services were never inserted, so run and stop have nothing to operate on,
	// and the listener must not open on a half-migrated database.
	if let Err(error) = &started
		&& cancelled_by_shutdown(&server.server, error)
	{
		signals.await?;

		return Ok(());
	}

	started?;
	async_run(server).await?;
	async_stop(server).await?;
	signals.await?;

	debug_info!("Exit runtime");
	Ok(())
}

/// Whether a failed startup is a stop request being honored rather than a
/// fault.
///
/// A cancelled startup exits zero, while a fault does not, and restart policies
/// keyed on failure act on the difference.
fn cancelled_by_shutdown(server: &CoreServer, error: &Error) -> bool {
	error.is_interrupted() && server.is_stopping()
}

/// Builds the services and runs every startup phase, inserting them on success.
///
/// The interrupted error a shutdown produces is passed through unchanged rather
/// than reported as a fault, so the caller can tell the two apart. Services are
/// inserted only on success, so a failed start leaves the slot empty.
#[cfg(any(not(tuwunel_mods), not(feature = "tuwunel_mods")))]
pub async fn async_start(server: &Arc<Server>) -> Result<Arc<Services>> {
	extern crate tuwunel_router as router;

	Ok(match router::start(&server.server).await {
		| Ok(services) => server
			.services
			.lock()
			.await
			.insert(services)
			.clone(),

		| Err(error) => {
			if cancelled_by_shutdown(&server.server, &error) {
				info!("Stop requested during startup; exiting before the server began serving.");
			} else {
				error!("Critical error starting server: {error}");
			}

			return Err(error);
		},
	})
}

/// Operate the server normally in release-mode static builds. This will start,
/// run and stop the server within the asynchronous runtime.
#[cfg(any(not(tuwunel_mods), not(feature = "tuwunel_mods")))]
pub async fn async_run(server: &Arc<Server>) -> Result {
	extern crate tuwunel_router as router;

	if let Err(error) = router::run(
		server
			.services
			.lock()
			.await
			.as_ref()
			.expect("services initialized"),
	)
	.await
	{
		error!("Critical error running server: {error}");
		return Err(error);
	}

	Ok(())
}

#[cfg(any(not(tuwunel_mods), not(feature = "tuwunel_mods")))]
pub async fn async_stop(server: &Arc<Server>) -> Result {
	extern crate tuwunel_router as router;

	if let Err(error) = router::stop(
		server
			.services
			.lock()
			.await
			.take()
			.expect("services initialized"),
	)
	.await
	{
		error!("Critical error stopping server: {error}");
		return Err(error);
	}

	Ok(())
}

/// Operate the server in developer-mode dynamic builds. This will start, run,
/// and hot-reload portions of the server as-needed before returning for an
/// actual shutdown. This is not available in release-mode or static builds.
#[cfg(all(tuwunel_mods, feature = "tuwunel_mods"))]
pub async fn async_exec(server: &Arc<Server>) -> Result {
	let mut starts = true;
	let mut reloads = true;
	while reloads {
		if let Err(error) = mods::open(server).await {
			error!("Loading router: {error}");
			return Err(error);
		}

		let result = mods::run(server, starts).await;
		if let Ok(result) = result {
			(starts, reloads) = result;
		}

		let force = !reloads || result.is_err();
		if let Err(error) = mods::close(server, force).await {
			error!("Unloading router: {error}");
			return Err(error);
		}

		if let Err(error) = result {
			error!("{error}");
			return Err(error);
		}
	}

	debug_info!("Exit runtime");
	Ok(())
}
