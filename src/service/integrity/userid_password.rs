//! Hashed sentinels in `userid_password`.
//!
//! An account with no password of its own stores the sentinel bare. An older writer
//! could hash it like any other password.

use std::{fmt, str::from_utf8};

use futures::Stream;
use tuwunel_core::{
	Result,
	smallstr::SmallString,
	utils::{TryReadyExt, hash::verify_password},
};
use tuwunel_database::KeyBuf;

use super::{Check, Report, ScanExt, tally};
use crate::{
	Services,
	users::{PASSWORD_SENTINEL, is_password_hash},
};

type Id = SmallString<[u8; 48]>;

pub(super) static CHECK: Check = Check {
	name: "sentinel-passwords",
	about: "Every userid_password row is text and none holds a hash of the sentinel",
	quiescent: false,
	walk: |services, limit| tally(scan(services), limit),
};

/// A `userid_password` row that is not what its writer stores.
///
/// The sentinel marks an account with no password of its own. Hashed, it reads as
/// a real password rather than that marker.
pub enum Finding {
	/// The stored hash verifies against the sentinel.
	Hashed {
		/// The user owning the row.
		user_id: Id,
	},

	/// The key or the value is not text.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},
}

/// Streams each password row hashing the sentinel, and each that is not text.
///
/// A value that is text but no hash is not reported, so neither is a hash in a
/// format this build cannot verify. Whether an identity provider is configured
/// plays no part: the row is the same either way.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	services.db["userid_password"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|(key, value)| Ok(classify(key, value)))
}

/// Classifies one row as [`scan`] does.
///
/// A repair reads the row a finding names again and passes it back through here
/// before writing, so detection and repair share one predicate.
pub(crate) fn classify(key: &[u8], value: &[u8]) -> Option<Finding> {
	match (from_utf8(key), from_utf8(value)) {
		| (Ok(user_id), Ok(value)) =>
			hashes_sentinel(value).then(|| Finding::Hashed { user_id: user_id.into() }),
		| _ => Some(Finding::Unparsable { key: KeyBuf::from_slice(key) }),
	}
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Hashed { .. } => "hashed-sentinel",
			| Self::Unparsable { .. } => "unparsable-row",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Hashed { user_id } => {
				write!(f, "password of {user_id} is a hash of the sentinel")
			},
			| Self::Unparsable { key } => write!(f, "row is not text: {}", key.escape_ascii()),
		}
	}
}

fn hashes_sentinel(value: &str) -> bool {
	is_password_hash(value) && verify_password(PASSWORD_SENTINEL, value).is_ok()
}
