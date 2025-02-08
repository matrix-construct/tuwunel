pub mod alloc;
pub mod config;
pub mod debug;
pub mod error;
pub mod info;
pub mod log;
pub mod metrics;
pub mod mods;
pub mod pdu;
pub mod server;
pub mod utils;

pub use ::arrayvec;
pub use ::http;
pub use ::ruma;
pub use ::smallstr;
pub use ::smallvec;
pub use ::toml;
pub use ::tracing;
pub use config::Config;
pub use error::Error;
pub use info::{rustc_flags_capture, version, version::version};
pub use pdu::{Event, PduBuilder, PduCount, PduEvent, PduId, RawPduId, StateKey};
pub use server::Server;
pub use utils::{ctor, dtor, implement, result, result::Result};

pub use crate as conduwuit_core;

rustc_flags_capture! {}

#[cfg(any(not(conduwuit_mods), not(feature = "conduwuit_mods")))]
pub mod mods {
	#[macro_export]
	macro_rules! mod_ctor {
		() => {};
	}
	#[macro_export]
	macro_rules! mod_dtor {
		() => {};
	}
}
