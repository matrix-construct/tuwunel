//! Provides shared runtime foundations for Tuwunel.
//!
//! The crate centralizes configuration, errors, Matrix data types, logging,
//! metrics, server lifecycle state, and reusable utilities. Higher-level
//! workspace crates consume these APIs through a shared foundational
//! dependency.
#![deny(missing_docs)]

pub mod alloc;
pub mod config;
pub mod debug;
pub mod error;
pub mod info;
pub mod log;
pub mod matrix;
pub mod metrics;
pub mod mods;
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
pub use server::{Progress, Server};
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
