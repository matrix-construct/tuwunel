//! Device-list reads for the sync tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code.

use serde_json::Value;
use tuwunel_core::ruma::UserId;

/// Whether a `/sync` and a sliding sync response list a user as changed.
///
/// The pair reads `(sync, sliding)`, so a failing check names the surface that
/// missed the user.
pub(crate) fn listed_on_both(sync: &Value, sliding: &Value, user_id: &UserId) -> (bool, bool) {
	let sync_listed = device_list_changed(&sync["device_lists"], user_id);
	let sliding_listed = device_list_changed(e2ee_device_lists(sliding), user_id);

	(sync_listed, sliding_listed)
}

/// Whether a sync response's `device_lists` object lists a user as changed.
///
/// `/sync` and the sliding sync e2ee extension carry the object under
/// different roots, so the caller hands in the object itself; a missing or
/// malformed list lists nobody.
pub(crate) fn device_list_changed(device_lists: &Value, user_id: &UserId) -> bool {
	device_lists["changed"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(Value::as_str)
		.any(|user| user == user_id.as_str())
}

/// The `device_lists` object of a sliding sync response's e2ee extension.
///
/// A response without the extension yields JSON null, which lists nobody.
pub(crate) fn e2ee_device_lists(response: &Value) -> &Value {
	&response["extensions"]["e2ee"]["device_lists"]
}
