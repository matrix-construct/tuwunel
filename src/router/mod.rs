#![type_length_limit = "32768"] //TODO: reduce me
#![expect(clippy::duration_suboptimal_units)] // remove after MSRV 1.91

mod layers;
mod request;
mod router;
mod run;
mod serve;

use std::{panic::AssertUnwindSafe, pin::Pin, sync::Arc};

use futures::{Future, FutureExt, TryFutureExt};
use log as _;
use tuwunel_core::{Error, Result, Server};
use tuwunel_service::Services;

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}
tuwunel_core::rustc_flags_capture! {}

#[unsafe(no_mangle)]
pub extern "Rust" fn start(
	server: &Arc<Server>,
) -> Pin<Box<dyn Future<Output = Result<Arc<Services>>> + Send>> {
	AssertUnwindSafe(run::start(server.clone()))
		.catch_unwind()
		.map_err(Error::from_panic)
		.unwrap_or_else(Err)
		.boxed()
}

#[unsafe(no_mangle)]
pub extern "Rust" fn stop(
	services: Arc<Services>,
) -> Pin<Box<dyn Future<Output = Result> + Send>> {
	AssertUnwindSafe(run::stop(services))
		.catch_unwind()
		.map_err(Error::from_panic)
		.unwrap_or_else(Err)
		.boxed()
}

#[unsafe(no_mangle)]
pub extern "Rust" fn run(
	services: &Arc<Services>,
) -> Pin<Box<dyn Future<Output = Result> + Send>> {
	AssertUnwindSafe(run::run(services.clone()))
		.catch_unwind()
		.map_err(Error::from_panic)
		.unwrap_or_else(Err)
		.boxed()
}
