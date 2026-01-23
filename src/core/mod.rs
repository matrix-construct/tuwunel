#![type_length_limit = "12288"]

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
pub use ::toml;
pub use ::tracing;
pub use config::Config;
pub use error::Error;
pub use info::{rustc_flags_capture, version, version::version};
pub use matrix::{
	Event, EventTypeExt, Pdu, PduCount, PduEvent, PduId, RoomVersion, pdu, state_res,
};
pub use server::Server;
pub use utils::{ctor, dtor, implement, result, result::Result};

pub use crate as tuwunel_core;

rustc_flags_capture! {}

#[cfg(any(not(tuwunel_mods), not(feature = "tuwunel_mods")))]
pub mod mods {
	use log as _;

	#[macro_export]
	macro_rules! mod_ctor {
		() => {};
	}
	#[macro_export]
	macro_rules! mod_dtor {
		() => {};
	}
}
