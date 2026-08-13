//! Information about the project. This module contains version, build, system,
//! etc information which can be queried by admins or used by developers.

pub mod cargo;
pub mod rustc;
pub mod version;

pub use tuwunel_macros::rustc_flags_capture;

/// Names the root crate containing this module.
///
/// The value is derived at compile time from `module_path!`. It includes the
/// full crate name before any Rust module separator.
pub const MODULE_ROOT: &str = const_str::split!(std::module_path!(), "::")[0];

/// Names the workspace crate-family prefix.
///
/// The value is the portion of [`MODULE_ROOT`] before its first underscore. It
/// is shared by crates following the workspace naming convention.
pub const CRATE_PREFIX: &str = const_str::split!(MODULE_ROOT, '_')[0];
