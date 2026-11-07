//! The keys of `roomuserid_joined`.
//!
//! A key joins a room id and a user id on one separator. A conduwuit-era writer
//! could double it.

use std::fmt;

use futures::Stream;
use tuwunel_core::{Result, utils::TryReadyExt};
use tuwunel_database::{KeyBuf, SEP, keyval::Buffer};

use super::{Check, Report, ScanExt, tally};
use crate::Services;

type Single = Buffer<64>;
type Count = Buffer<8>;

pub(super) static CHECK: Check = Check {
	name: "joined-keys",
	about: "Every roomuserid_joined key has one separator where its room id ends",
	quiescent: false,
	walk: |services, limit| tally(scan(services), limit),
};

/// A `roomuserid_joined` key whose first separator is not a lone one.
///
/// Neither shape decodes as a room and a user. A doubled separator can be closed
/// up, while a key with none does not say where its room id ends.
pub enum Finding {
	/// The separator after the room id is written twice.
	Doubled {
		/// The stored key.
		key: KeyBuf,

		/// The key with its first separator written once.
		single: Single,

		/// The stored value, which belongs under the corrected key.
		value: Count,
	},

	/// The key has no separator at all.
	Unseparated {
		/// The stored key.
		key: KeyBuf,
	},
}

/// Streams each `roomuserid_joined` key with a malformed first separator.
///
/// Only the first separator is examined. The defect is visible in the row alone,
/// so any later row is unaffected by a repair of this one.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	services.db["roomuserid_joined"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|(key, value)| Ok(classify(key, value)))
}

/// Classifies one row as [`scan`] does.
///
/// A repair reads the row a finding names again and passes it back through here
/// before writing, so detection and repair share one predicate.
pub(crate) fn classify(key: &[u8], value: &[u8]) -> Option<Finding> {
	let Some(at) = key.iter().position(|&byte| byte == SEP) else {
		return Some(Finding::Unseparated { key: KeyBuf::from_slice(key) });
	};

	let (room_id, rest) = key.split_at(at);

	rest.starts_with(&[SEP, SEP]).then(|| {
		let single = room_id
			.iter()
			.chain(rest.iter().skip(1))
			.copied()
			.collect();

		Finding::Doubled {
			key: KeyBuf::from_slice(key),
			single,
			value: Count::from_slice(value),
		}
	})
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Doubled { .. } => "doubled-separator",
			| Self::Unseparated { .. } => "missing-separator",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Doubled { key, .. } => {
				write!(f, "doubled separator in {}", key.escape_ascii())
			},
			| Self::Unseparated { key } => {
				write!(f, "no separator in {}", key.escape_ascii())
			},
		}
	}
}
