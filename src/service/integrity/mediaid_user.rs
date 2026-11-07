//! The rows of `mediaid_user`.
//!
//! A key is a media id and the user who uploaded it. One check covers the key layout
//! that came before, the other rows that do not name this server's media.

use std::{fmt, str::from_utf8};

use futures::Stream;
use ruma::{MxcUri, OwnedMxcUri, OwnedUserId, UserId};
use tuwunel_core::{
	Result,
	itertools::Itertools,
	utils::{BoolExt, TryReadyExt},
};
use tuwunel_database::{KeyBuf, SEP, keyval::Buffer};

use super::{Check, Report, ScanExt, tally};
use crate::Services;

type Value = Buffer<48>;

pub(super) static LEGACY: Check = Check {
	name: "media-uploader-keys",
	about: "Every mediaid_user key has a separator",
	quiescent: false,
	walk: |services, limit| tally(legacy(services), limit),
};

pub(super) static REMOTE: Check = Check {
	name: "remote-media-uploaders",
	about: "Every separated mediaid_user key pairs a valid MXC URI here with a user id",
	quiescent: false,
	walk: |services, limit| tally(remote(services), limit),
};

/// A `mediaid_user` key from before keys carried the uploader.
///
/// Such a key holds only the media id, with the uploader in the value alone, so a
/// read by the media id's prefix misses the row.
pub enum Legacy {
	/// The value names the uploader, so the key can carry it.
	Upgradable {
		/// The stored key.
		key: KeyBuf,

		/// The uploader the value names.
		user_id: OwnedUserId,
	},

	/// The value is not a user id, so nothing can rebuild the row.
	Unowned {
		/// The stored key.
		key: KeyBuf,

		/// The stored value.
		value: Value,
	},
}

/// A `mediaid_user` row with no business being in the column.
///
/// The column records who uploaded this server's media, which rules out media
/// of another server and whatever is not media at all.
pub enum Remote {
	/// The media belongs to another server.
	Foreign {
		/// The stored key.
		key: KeyBuf,

		/// The media.
		mxc: OwnedMxcUri,
	},

	/// The media id is not a valid MXC URI.
	Invalid {
		/// The stored key.
		key: KeyBuf,

		/// The media id.
		mxc: OwnedMxcUri,
	},

	/// The key has a separator but is not exactly a media id and a valid user id.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},
}

/// Streams each `mediaid_user` key with no separator.
///
/// The defect is visible in the row alone, so any later row is unaffected by a
/// repair of this one. A key with a separator is never reported here, whatever
/// surrounds it.
#[tracing::instrument(level = "trace", skip_all)]
pub fn legacy(services: &Services) -> impl Stream<Item = Result<Legacy>> + Send + '_ {
	services.db["mediaid_user"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|(key, value)| Ok(classify_legacy(key, value)))
}

/// Classifies one row as [`legacy`] does.
///
/// A repair reads the row a finding names again and passes it back through here
/// before writing, so detection and repair share one predicate.
pub(crate) fn classify_legacy(key: &[u8], value: &[u8]) -> Option<Legacy> {
	key.contains(&SEP)
		.or(|| legacy_finding(key, value))
}

/// Streams each separated `mediaid_user` key that fails to name this server's media.
///
/// A key with no separator is left to [`legacy`], which reports every such row, so
/// no row is reported twice. The server test reads configuration and the media id,
/// so any later row is unaffected by a repair of this one.
#[tracing::instrument(level = "trace", skip_all)]
pub fn remote(services: &Services) -> impl Stream<Item = Result<Remote>> + Send + '_ {
	services.db["mediaid_user"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter(|(key, _)| key.contains(&SEP))
		.ready_try_filter_map(|(key, _)| Ok(remote_finding(services, key)))
}

impl Report for Legacy {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Upgradable { .. } => "legacy-key",
			| Self::Unowned { .. } => "unowned-key",
		}
	}
}

impl fmt::Display for Legacy {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Upgradable { key, user_id } => {
				write!(f, "{} has no uploader in its key; {user_id} owns it", key.escape_ascii())
			},
			| Self::Unowned { key, .. } => {
				write!(f, "{} has no uploader in key or value", key.escape_ascii())
			},
		}
	}
}

impl Report for Remote {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Foreign { .. } => "remote-media",
			| Self::Invalid { .. } => "invalid-mxc",
			| Self::Unparsable { .. } => "unparsable-key",
		}
	}
}

impl fmt::Display for Remote {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Foreign { mxc, .. } => write!(f, "{mxc} is another server's media"),
			| Self::Invalid { mxc, .. } => write!(f, "{mxc} is not a valid MXC URI"),
			| Self::Unparsable { key } => write!(f, "key cannot be read: {}", key.escape_ascii()),
		}
	}
}

fn legacy_finding(key: &[u8], value: &[u8]) -> Legacy {
	let user_id = from_utf8(value)
		.ok()
		.and_then(|value| <&UserId>::try_from(value).ok());

	match user_id {
		| None => Legacy::Unowned {
			key: KeyBuf::from_slice(key),
			value: Value::from_slice(value),
		},
		| Some(user_id) => Legacy::Upgradable {
			key: KeyBuf::from_slice(key),
			user_id: user_id.to_owned(),
		},
	}
}

fn remote_finding(services: &Services, key: &[u8]) -> Option<Remote> {
	let Some(mxc) = media_id(key) else {
		return Some(Remote::Unparsable { key: KeyBuf::from_slice(key) });
	};

	let key = || KeyBuf::from_slice(key);

	match mxc.parts() {
		| Err(_) => Some(Remote::Invalid { key: key(), mxc: mxc.to_owned() }),
		| Ok(parts) if !services.globals.server_is_ours(parts.server_name) =>
			Some(Remote::Foreign { key: key(), mxc: mxc.to_owned() }),
		| Ok(_) => None,
	}
}

/// Reads the media id of a key holding exactly a media id and a user id.
///
/// The key is split here rather than decoded by its tuple type, because a decoder
/// asserts the record count in debug builds and a damaged key is what this walk
/// is for.
fn media_id(key: &[u8]) -> Option<&MxcUri> {
	let (mxc, user_id) = key.split(|&byte| byte == SEP).collect_tuple()?;
	let mxc = from_utf8(mxc).ok()?;
	let user_id = from_utf8(user_id).ok()?;

	<&UserId>::try_from(user_id)
		.is_ok()
		.then(|| mxc.into())
}
