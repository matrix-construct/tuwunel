//! List bodies and count assertions shared by the tests that check sliding
//! sync lists.
//!
//! A test binary declares this file by path, beside the sliding sync module,
//! only when it requests lists, so that module's other users compile none of
//! it.

use serde_json::{Value, json};
use tuwunel_core::{Result, err, utils::BoolExt};

/// One list over the first hundred rooms, admitting those `filters` match.
///
/// It asks for no timeline or state, since these tests read which rooms a list
/// admits and how many, not what the rooms hold.
pub(crate) fn list(filters: &Value) -> Value {
	json!({
		"ranges": [[0, 99]],
		"required_state": [],
		"timeline_limit": 0,
		"filters": filters,
	})
}

/// Fails unless a reply reports the list with exactly the expected count.
///
/// An absent list fails as a wrong count does, since matrix-rust-sdk waits on
/// the field to learn that a list is fully loaded. The failure opens with
/// `context`, naming the round that made the request.
pub(crate) fn expect_count(response: &Value, list: &str, expected: u64, context: &str) -> Result {
	let count = response["lists"][list]["count"].as_u64();

	BoolExt::ok_or_else(count == Some(expected), || {
		err!("{context}: list {list} reported {count:?} rather than {expected}")
	})
}
