mod and_then_ref;
mod debug_inspect;
mod filter;
mod flat_ok;
mod inspect_log;
mod into_is_ok;
mod is_err_or;
mod log_debug_err;
mod log_err;
mod map_expect;
mod map_ref;
mod not_found;
mod unwrap_infallible;
mod unwrap_or_err;

pub use self::{
	and_then_ref::AndThenRef,
	debug_inspect::DebugInspect,
	filter::Filter,
	flat_ok::FlatOk,
	inspect_log::{ErrDebugLog, ErrLog},
	into_is_ok::IntoIsOk,
	is_err_or::IsErrOr,
	log_debug_err::LogDebugErr,
	log_err::LogErr,
	map_expect::MapExpect,
	map_ref::MapRef,
	not_found::NotFound,
	unwrap_infallible::UnwrapInfallible,
	unwrap_or_err::UnwrapOrErr,
};

pub type Result<T = (), E = crate::Error> = std::result::Result<T, E>;
