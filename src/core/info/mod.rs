//! Information about the project. This module contains version, build, system,
//! etc information which can be queried by admins or used by developers.

pub mod cargo;
pub mod rustc;
pub mod version;

pub use tuwunel_macros::rustc_flags_capture;

pub const MODULE_ROOT: &str = const_str::split!(std::module_path!(), "::")[0];
pub const CRATE_PREFIX: &str = const_str::split!(MODULE_ROOT, '_')[0];
