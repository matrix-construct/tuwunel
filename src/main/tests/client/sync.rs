//! `/sync` response reads for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code.

use serde_json::Value;
use tuwunel_core::{Result, err, ruma::RoomId};

/// One joined room's section of a `/sync` response.
///
/// A response that leaves the room out is a test failure, so the error names
/// the room.
pub(crate) fn joined_room<'a>(response: &'a Value, room_id: &RoomId) -> Result<&'a Value> {
	response["rooms"]["join"]
		.get(room_id.as_str())
		.ok_or_else(|| err!("sync response omitted joined room {room_id}"))
}
