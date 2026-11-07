//! The timestamp index `roomid_tscount_pducount`.
//!
//! Each timeline event has one row keyed by its room, its timestamp and its
//! biased count, holding the count, so a timestamp seeks to the events nearest
//! it. The index derives from `pduid_pdu`, and its rebuild writes over it without
//! clearing it first.

use std::{fmt, sync::Arc};

use futures::Stream;
use ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId};
use serde::Deserialize;
use tuwunel_core::{
	Result, implement,
	result::NotFound,
	utils::{TryReadyExt, stream::TryBroadbandExt},
};
use tuwunel_database::{KeyBuf, KeyVal, Map};
use tuwunel_matrix::pdu::RawPduId;

use super::{Check, Report, ScanExt, tally};
use crate::{Services, rooms::timeline::bias_count};

type Key<'a> = (&'a RoomId, u64, u64);

pub(super) static CHECK: Check = Check {
	name: "timestamp-index",
	about: "Every timeline event has its roomid_tscount_pducount row, holding its count",
	quiescent: true,
	walk: |services, limit| tally(scan(services), limit),
};

/// A timeline event whose timestamp index row is absent or wrong.
///
/// Such an event is out of reach of a seek by timestamp. Only an event row whose
/// key is not a PDU id gives no row to look for.
pub enum Finding {
	/// The event has no row in the index.
	Missing(Row),

	/// The event's row holds a count other than the event's.
	Mismatched(Row),

	/// The event row's key is not a PDU id.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},
}

/// The row one timeline event gives the index.
///
/// Its key is the room, the timestamp and the biased count, and its value is the
/// count.
pub struct Row {
	/// The event the row indexes.
	pub pdu_id: RawPduId,

	/// The event's room.
	pub room_id: OwnedRoomId,

	/// The event's origin timestamp, in milliseconds.
	pub ts: u64,
}

/// What one event row gives the index.
///
/// An event row whose key is not a PDU id gives no row to write, but the check
/// reports it.
pub(crate) enum Expected {
	/// The event's row.
	Row(Row),

	/// The event row's key is not a PDU id.
	Unparsable(KeyBuf),
}

#[derive(Deserialize)]
struct PduRoomTs {
	room_id: OwnedRoomId,
	origin_server_ts: MilliSecondsSinceUnixEpoch,
}

/// Streams each timeline event whose index row is absent or holds another count.
///
/// The two columns sort differently, so each event's row is read by key. Only
/// coverage is checked: a row naming no stored event is not reported, because
/// the rebuild removes none either. An event whose value does not decode gives no
/// row here, and is left to the relation index check, which decodes every event.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	let index = &services.db["roomid_tscount_pducount"];

	expected(services)
		.broad_and_then(async |expected| match expected {
			| Expected::Row(row) => verify(index, row).await,
			| Expected::Unparsable(key) => Ok(Some(Finding::Unparsable { key })),
		})
		.ready_try_filter_map(Ok)
}

/// Streams the row each timeline event gives the index.
///
/// The check compares these with the index and the rebuild writes every one, so
/// the two agree on what the index should hold.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn expected(services: &Services) -> impl Stream<Item = Result<Expected>> + Send + '_ {
	services.db["pduid_pdu"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|row| Ok(derive(row)))
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Missing(_) => "missing-row",
			| Self::Mismatched(_) => "mismatched-count",
			| Self::Unparsable { .. } => "unparsable-key",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Missing(Row { pdu_id, room_id, ts }) => {
				write!(f, "event {pdu_id:?} in {room_id} at {ts} has no index row")
			},
			| Self::Mismatched(Row { pdu_id, room_id, ts }) => {
				write!(
					f,
					"index row of event {pdu_id:?} in {room_id} at {ts} holds another count"
				)
			},
			| Self::Unparsable { key } => {
				write!(f, "event key is not a PDU id: {}", key.escape_ascii())
			},
		}
	}
}

fn derive((key, value): KeyVal<'_>) -> Option<Expected> {
	let Some(pdu_id) = RawPduId::try_from_bytes(key) else {
		return Some(Expected::Unparsable(KeyBuf::from_slice(key)));
	};

	let PduRoomTs { room_id, origin_server_ts } = serde_json::from_slice(value).ok()?;
	let ts = origin_server_ts.get().into();

	Some(Expected::Row(Row { pdu_id, room_id, ts }))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn verify(index: &Arc<Map>, row: Row) -> Result<Option<Finding>> {
	let stored = index
		.qry(&row.key())
		.await
		.optional()?
		.map(|count| *count == row.pdu_id.count());

	let finding = match stored {
		| None => Some(Finding::Missing(row)),
		| Some(false) => Some(Finding::Mismatched(row)),
		| Some(true) => None,
	};

	Ok(finding)
}

/// The key of the index row.
///
/// Its count is biased so that backfilled events sort before the rest.
#[implement(Row)]
#[inline]
pub(crate) fn key(&self) -> Key<'_> { (&self.room_id, self.ts, bias_count(self.pdu_id.count())) }
