//! The thread pointers in `threadrootid_latestcount`.
//!
//! A pointer holds the count of the one `threadactivityid_rootid` row that lists
//! its thread, so a room's thread list reads activity rows newest first and skips
//! any row a pointer has moved past.

use std::{fmt, sync::Arc};

use futures::Stream;
use tuwunel_core::{
	Result,
	result::NotFound,
	utils::{TryReadyExt, stream::TryBroadbandExt},
};
use tuwunel_database::{KeyBuf, Map};
use tuwunel_matrix::{PduCount, RawPduId};

use super::{Check, Report, ScanExt, tally};
use crate::{Services, rooms::threads::activity_key};

type Pointer = (RawPduId, PduCount);

pub(super) static CHECK: Check = Check {
	name: "thread-activity",
	about: "Every thread pointer has the activity row it points at, naming its root",
	quiescent: true,
	walk: |services, limit| tally(scan(services), limit),
};

/// A thread pointer without the activity row it points at.
///
/// Its thread is then missing from its room's thread list. The rebuild of the
/// thread activity index writes every pointer afresh beside its row.
pub enum Finding {
	/// No activity row sits at the pointer's count.
	Dangling {
		/// The thread root.
		root_id: RawPduId,

		/// The count the pointer holds.
		count: PduCount,
	},

	/// The activity row at the pointer's count names another root.
	Misdirected {
		/// The thread root.
		root_id: RawPduId,

		/// The count the pointer holds.
		count: PduCount,

		/// The root the activity row names.
		named: KeyBuf,
	},

	/// The pointer's key is not a PDU id, or its value not a count.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},
}

/// Streams each pointer whose activity row is absent or names another root.
///
/// Only the pointer is checked. Activity rows behind it are dead and are reaped
/// as the list is read, and a pointer need not name the newest reply, since a
/// redaction moves it only when the thread empties.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	let activity = &services.db["threadactivityid_rootid"];

	services.db["threadrootid_latestcount"]
		.raw_scan()
		.scanned(&services.server)
		.ready_and_then(|(key, value)| {
			Ok(pointer(key, value).ok_or_else(|| KeyBuf::from_slice(key)))
		})
		.broad_and_then(async |pointer| match pointer {
			| Ok(pointer) => verify(activity, pointer).await,
			| Err(key) => Ok(Some(Finding::Unparsable { key })),
		})
		.ready_try_filter_map(Ok)
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Dangling { .. } => "dangling-pointer",
			| Self::Misdirected { .. } => "misdirected-pointer",
			| Self::Unparsable { .. } => "unparsable-pointer",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Dangling { root_id, count } => {
				write!(f, "pointer of thread root {root_id:?} at {count:?} has no activity row")
			},
			| Self::Misdirected { root_id, count, named } => write!(
				f,
				"activity row at {count:?} for thread root {root_id:?} names {}",
				named.escape_ascii()
			),
			| Self::Unparsable { key } => {
				write!(f, "thread pointer cannot be read: {}", key.escape_ascii())
			},
		}
	}
}

fn pointer(key: &[u8], value: &[u8]) -> Option<Pointer> {
	let root_id = RawPduId::try_from_bytes(key)?;
	let count = value.try_into().map(u64::from_be_bytes).ok()?;

	Some((root_id, PduCount::from_unsigned(count)))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn verify(activity: &Arc<Map>, (root_id, count): Pointer) -> Result<Option<Finding>> {
	let named = activity
		.get(&activity_key(root_id, count))
		.await
		.optional()?
		.map(|named| {
			named
				.ne(root_id.as_bytes())
				.then(|| KeyBuf::from_slice(&named))
		});

	let finding = match named {
		| None => Some(Finding::Dangling { root_id, count }),
		| Some(Some(named)) => Some(Finding::Misdirected { root_id, count, named }),
		| Some(None) => None,
	};

	Ok(finding)
}
