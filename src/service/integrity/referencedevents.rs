use std::{fmt, str::from_utf8};

use futures::Stream;
use tuwunel_core::{
	Result,
	smallstr::SmallString,
	utils::{BoolExt, TryReadyExt},
};
use tuwunel_database::{KeyBuf, SEP};

use super::{Check, Report, ScanExt, tally};
use crate::Services;

/// Inline budget for one of the ids a key joins.
///
/// A hashed event id fits, while a room id on a long server name spills to the
/// heap, which a finding can afford.
type Id = SmallString<[u8; 48]>;

pub(super) static CHECK: Check = Check {
	name: "referenced-events",
	about: "Every referencedevents key separates its room and event ids",
	quiescent: false,
	walk: |services, limit| tally(scan(services), limit),
};

/// A `referencedevents` key with no separator between its room and event ids.
///
/// Such a key reads as an unknown room, so the event's reference is lost until the
/// key is rewritten. Only a key naming where the event id begins can be.
pub enum Finding {
	/// The key joins the two ids, and the event id's sigil shows where to split it.
	Unseparated {
		/// The stored key.
		key: KeyBuf,

		/// The part of the key before the event id.
		room_id: Id,

		/// The event id ending the key.
		event_id: Id,
	},

	/// The key has no sigil to split at, or is not text.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},
}

/// Streams each `referencedevents` key missing its separator.
///
/// The defect is visible in the key alone, so any later row is unaffected by a
/// repair of this one.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	services.db["referencedevents"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|(key, _)| Ok(key.contains(&SEP).or(|| finding(key))))
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Unseparated { .. } => "missing-separator",
			| Self::Unparsable { .. } => "unparsable-key",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Unseparated { room_id, event_id, .. } => {
				write!(f, "no separator between {room_id} and {event_id}")
			},
			| Self::Unparsable { key } => write!(f, "key cannot be split: {key:?}"),
		}
	}
}

fn finding(key: &[u8]) -> Finding {
	let parts = from_utf8(key)
		.ok()
		.and_then(|key| key.find('$').map(|at| key.split_at(at)));

	match parts {
		| None => Finding::Unparsable { key: KeyBuf::from_slice(key) },
		| Some((room_id, event_id)) => Finding::Unseparated {
			key: KeyBuf::from_slice(key),
			room_id: room_id.into(),
			event_id: event_id.into(),
		},
	}
}
