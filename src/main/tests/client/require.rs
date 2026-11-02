//! Condition checks for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code.

use tuwunel_core::{Result, err, utils::BoolExt};

/// Pass when a checked condition holds, and fail with the given message when not.
///
/// The message names what went wrong, so a failing case reads as its cause.
pub(crate) fn require(holds: bool, failure: &str) -> Result {
	holds
		.into_option()
		.ok_or_else(|| err!("{failure}"))
}
