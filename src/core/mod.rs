//! Provides shared runtime foundations for Tuwunel.
//!
//! The crate centralizes configuration, errors, Matrix data types, logging,
//! metrics, server lifecycle state, and reusable utilities. Higher-level
//! workspace crates consume these APIs through a shared foundational
//! dependency.
#![deny(missing_docs)]

pub mod alloc;
/// Loads and validates server configuration.
///
/// Configuration types preserve startup sources for reloads and expose typed
/// settings to the rest of the workspace. Field documentation also supplies the
/// generated example configuration.
pub mod config;
pub mod debug;
/// Defines shared error and result types.
///
/// Errors retain protocol and transport context across crate boundaries. The
/// module also provides the macros used to construct and report them.
pub mod error;
pub mod info;
/// Configures structured logging and in-memory capture.
///
/// The module builds tracing layers for supported output targets. It also
/// exposes scoped capture and reload controls to the server.
pub mod log;
pub mod matrix;
/// Collects task, runtime, and request metrics.
///
/// Runtime instrumentation is enabled when the required Tokio facilities are
/// available. Snapshot helpers expose interval data to diagnostics and
/// telemetry.
pub mod metrics;
pub mod mods;
/// Tracks server lifecycle state and its runtime handle.
///
/// The server coordinates reload, restart, and shutdown notifications. Shared
/// services use its state to stop work promptly during teardown.
pub mod server;
pub mod utils;

pub use ::arrayvec;
pub use ::either;
pub use ::http;
pub use ::itertools;
pub use ::jsonwebtoken as jwt;
pub use ::ruma;
pub use ::smallstr;
pub use ::smallvec;
pub use ::tokio_metrics;
pub use ::toml;
pub use ::tracing;
pub use config::Config;
pub use error::Error;
pub use info::{rustc_flags_capture, version, version::version};
pub use matrix::{Event, EventTypeExt, Pdu, PduCount, PduEvent, PduId, RoomVersion, pdu};
pub use server::Server;
pub use utils::{async_noinline, ctor, dtor, implement, result, result::Result};

pub use crate as tuwunel_core;

rustc_flags_capture! {}

#[cfg(any(not(tuwunel_mods), not(feature = "tuwunel_mods")))]
/// Provides inert dynamic-module hooks when module loading is unavailable.
///
/// The fallback keeps call sites portable across builds with and without the
/// dynamic module feature. Its exported macros intentionally perform no work.
pub mod mods {
	use log as _;

	#[macro_export]
	/// Defines an empty module-constructor hook.
	///
	/// Builds without dynamic module support can keep the same `mod_ctor!`
	/// invocation as enabled builds. Invoking it emits no constructor.
	macro_rules! mod_ctor {
		() => {};
	}
	#[macro_export]
	/// Defines an empty module-destructor hook.
	///
	/// Builds without dynamic module support can keep the same `mod_dtor!`
	/// invocation as enabled builds. Invoking it emits no destructor.
	macro_rules! mod_dtor {
		() => {};
	}
}
