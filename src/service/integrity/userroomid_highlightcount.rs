//! The keys of `userroomid_highlightcount`.
//!
//! Conduit opened `roomuserid_lastnotificationread` on this column's tree, so a
//! database it wrote holds both stores here. A highlight count's key leads with
//! the user and a last-read marker's with the room.

use std::fmt;

use futures::Stream;
use tuwunel_core::{Result, utils::TryReadyExt};
use tuwunel_database::{KeyBuf, keyval::Buffer};

use super::{Check, Report, ScanExt, tally};
use crate::Services;

type Value = Buffer<8>;

const ROOM_SIGIL: &[u8] = b"!";

pub(super) static CHECK: Check = Check {
	name: "highlight-keys",
	about: "No userroomid_highlightcount key leads with a room id",
	quiescent: false,
	walk: |services, limit| tally(scan(services), limit),
};

/// A last-notification-read marker Conduit stored in the highlight-count column.
///
/// Its key leads with the room, as the keys of `roomuserid_lastnotificationread`
/// do, so no reader of either column finds it until the row moves there.
pub struct Finding {
	/// The stored key.
	pub key: KeyBuf,

	/// The stored marker.
	pub value: Value,
}

/// Streams each `userroomid_highlightcount` row whose key leads with a room id.
///
/// Keys sort as bytes, so such rows sit together near the front of the column:
/// any key sorting below them is skipped, as this check does not cover it, and
/// the walk ends at the first key past them, so a native column costs one row.
/// The defect is visible in the key alone, so a repair of one row leaves the rest.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	services.db["userroomid_highlightcount"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_skip_while(|(key, _)| Ok(*key < ROOM_SIGIL))
		.ready_try_take_while(|(key, _)| Ok(key.starts_with(ROOM_SIGIL)))
		.ready_and_then(|(key, value)| {
			Ok(Finding {
				key: KeyBuf::from_slice(key),
				value: Value::from_slice(value),
			})
		})
}

impl Report for Finding {
	fn kind(&self) -> &'static str { "room-keyed-row" }
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "last-read marker in the highlight-count column: {}", self.key.escape_ascii())
	}
}
