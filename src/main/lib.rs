#![type_length_limit = "4096"] //TODO: reduce me

pub mod logging;
pub mod mods;
pub mod restart;
pub mod sentry;
pub mod server;
pub mod signals;

use std::sync::Arc;

use tuwunel_core::{
	Result, Runtime, debug_info, error, mod_ctor, mod_dtor, runtime::shutdown,
	rustc_flags_capture,
};

pub use self::server::Server;

mod_ctor! {}
mod_dtor! {}
rustc_flags_capture! {}

pub fn exec(server: &Arc<Server>, runtime: Runtime) -> Result {
	run(server, &runtime)?;
	shutdown(&server.server, runtime)
}

pub fn run(server: &Arc<Server>, runtime: &Runtime) -> Result {
	runtime.spawn(signals::enable(server.clone()));
	runtime.block_on(run_async(server))
}

/// Operate the server normally in release-mode static builds. This will start,
/// run and stop the server within the asynchronous runtime.
#[cfg(any(not(tuwunel_mods), not(feature = "tuwunel_mods")))]
#[tracing::instrument(
    name = "main",
    parent = None,
    skip_all
)]
pub async fn run_async(server: &Arc<Server>) -> Result {
	extern crate tuwunel_router as router;

	match router::start(&server.server).await {
		| Ok(services) => server.services.lock().await.insert(services),
		| Err(error) => {
			error!("Critical error starting server: {error}");
			return Err(error);
		},
	};

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

	debug_info!("Exit runtime");
	Ok(())
}

/// Operate the server in developer-mode dynamic builds. This will start, run,
/// and hot-reload portions of the server as-needed before returning for an
/// actual shutdown. This is not available in release-mode or static builds.
#[cfg(all(tuwunel_mods, feature = "tuwunel_mods"))]
pub async fn run_async(server: &Arc<Server>) -> Result {
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
