//! Leftover rows in `readreceiptid_readreceipt`.
//!
//! A key is a room id, a stream count and a user id, then a thread kind on any row
//! written since receipts became thread-aware. An older writer could leave a user's
//! previous receipt behind.

use std::{fmt, mem::replace, str::from_utf8};

use futures::Stream;
use ruma::{RoomId, UserId};
use tuwunel_core::{
	Result,
	smallstr::SmallString,
	utils::{ReadyExt, TryReadyExt},
};
use tuwunel_database::{KeyBuf, SEP};

use super::{Check, Report, ScanExt, tally};
use crate::Services;

type Id = SmallString<[u8; 48]>;
type Holder = (Id, Id);
type Row<'a> = (&'a RoomId, &'a UserId, bool);

const COUNT: usize = size_of::<u64>();

pub(super) static CHECK: Check = Check {
	name: "read-receipts",
	about: "Receipt keys read back, and none lacking a thread kind repeats the holder before it",
	quiescent: false,
	walk: |services, limit| tally(scan(services), limit),
};

/// A `readreceiptid_readreceipt` row left behind by an earlier receipt.
///
/// A row from before thread-aware receipts ends at its user. One directly after
/// a row of the same room and user is an entry its writer failed to delete.
pub enum Finding {
	/// The row repeats the room and user of the row before it.
	Repeated {
		/// The stored key.
		key: KeyBuf,

		/// The room of both rows.
		room_id: Id,

		/// The user of both rows.
		user_id: Id,
	},

	/// The key does not split into a room, a count and a user.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},
}

/// Streams each repeated row and each key that cannot be read.
///
/// Only adjacent rows are compared, in key order, and the first row of a run is
/// taken as the keeper. A key with a thread kind is compared against but never
/// reported, because one user holds a row per thread, and a pre-thread row is kept
/// even where a thread-aware row of the same count follows it. An unreadable key
/// breaks adjacency, so the rows either side of it are not called repeats.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	services.db["readreceiptid_readreceipt"]
		.raw_scan()
		.scanned(&services.server)
		.ready_scan(None, |last, row| {
			row.map(|(key, _)| {
				let row = parse(key);
				let repeated = advance(last, row);

				finding(key, row, repeated)
			})
			.into()
		})
		.ready_try_filter_map(Ok)
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Repeated { .. } => "repeated-receipt",
			| Self::Unparsable { .. } => "unparsable-key",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Repeated { room_id, user_id, .. } => {
				write!(f, "repeated receipt of {user_id} in {room_id}")
			},
			| Self::Unparsable { key } => write!(f, "key cannot be read: {}", key.escape_ascii()),
		}
	}
}

/// Reads a key's room and user, and whether a thread kind follows the user.
///
/// The key is split here rather than decoded by its tuple type, because a decoder
/// asserts the layout in debug builds and a damaged key is what this walk is for.
/// A kind the receipt reader could not decode makes the whole key unreadable.
fn parse(key: &[u8]) -> Option<Row<'_>> {
	let (room_id, rest) = split(key)?;
	let (_count, rest) = rest.split_first_chunk::<COUNT>()?;
	let rest = rest.strip_prefix(&[SEP])?;
	let (user_id, kind) =
		split(rest).map_or((rest, None), |(user_id, kind)| (user_id, Some(kind)));

	let room_id = from_utf8(room_id).ok()?.try_into().ok()?;
	let user_id = from_utf8(user_id).ok()?.try_into().ok()?;

	kind.is_none_or(readable)
		.then_some((room_id, user_id, kind.is_some()))
}

fn split(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
	let at = bytes.iter().position(|&byte| byte == SEP)?;
	let (head, tail) = bytes.split_at(at);

	Some((head, tail.strip_prefix(&[SEP])?))
}

/// Whether a thread kind decodes as the receipt reader takes it.
///
/// The reader takes the kind as text, and tolerates one separator after it.
fn readable(kind: &[u8]) -> bool { from_utf8(kind.strip_suffix(&[SEP]).unwrap_or(kind)).is_ok() }

/// Moves the walk on to a row, reporting whether it has the last row's holder.
///
/// An unreadable row leaves no holder behind, which is how it breaks adjacency.
fn advance(last: &mut Option<Holder>, row: Option<Row<'_>>) -> bool {
	let previous = replace(last, row.map(holder));

	previous.is_some_and(|previous| last.as_ref().eq(&Some(&previous)))
}

fn holder((room_id, user_id, _): Row<'_>) -> Holder {
	(room_id.as_str().into(), user_id.as_str().into())
}

fn finding(key: &[u8], row: Option<Row<'_>>, repeated: bool) -> Option<Finding> {
	match row {
		| None => Some(Finding::Unparsable { key: KeyBuf::from_slice(key) }),
		| Some((room_id, user_id, false)) if repeated => Some(Finding::Repeated {
			key: KeyBuf::from_slice(key),
			room_id: room_id.as_str().into(),
			user_id: user_id.as_str().into(),
		}),
		| Some(_) => None,
	}
}
