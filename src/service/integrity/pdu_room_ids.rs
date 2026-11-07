//! The `room_id` of stored events.
//!
//! Readers decode an event together with its room, so an event without one fails
//! to read. A room version 12 create event imported from Conduit omits it, since
//! its room derives from the event's own id (MSC4291), whether it was stored in
//! the timeline or as an outlier.

use std::{collections::BTreeMap, fmt, str::from_utf8};

use futures::{Stream, StreamExt, TryStreamExt, stream::once};
use ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedRoomId, RoomId};
use serde::{Deserialize, de::IgnoredAny};
use tuwunel_core::{
	Result,
	utils::{BoolExt, TryReadyExt},
};
use tuwunel_database::KeyBuf;

use super::{Check, Report, ScanExt, tally};
use crate::{Services, rooms::short::ShortRoomId};

type Rooms = BTreeMap<ShortRoomId, OwnedRoomId>;

pub(super) static CHECK: Check = Check {
	name: "pdu-room-ids",
	about: "Every JSON event in pduid_pdu and every event in eventid_outlierpdu carries a \
	        room_id",
	quiescent: false,
	walk: |services, limit| tally(scan(services), limit),
};

/// The `room_id` of a stored event, read without the rest of it.
///
/// Every row is read, so only an event this shows to lack its room pays for the
/// full parse.
#[derive(Deserialize)]
struct Probe {
	room_id: Option<IgnoredAny>,
}

/// A column holding stored events.
///
/// A finding names its column, so the repair reads the event back from the
/// column it came from, and an outlier's room derives from its key.
#[derive(Clone, Copy)]
pub enum Column {
	/// The timeline, `pduid_pdu`, whose keys lead with the room's short id.
	Timeline,

	/// The outliers, `eventid_outlierpdu`, keyed by event id.
	Outliers,
}

/// A stored event without its `room_id`, or an outlier that cannot be read.
///
/// The repair writes the room into an event whose room is known and leaves the
/// others as they are.
pub enum Finding {
	/// The event lacks its room, and its key or its own id names that room.
	Missing {
		/// The column holding the event.
		column: Column,

		/// The stored key.
		key: KeyBuf,

		/// The room the event belongs to.
		room_id: OwnedRoomId,

		/// The stored event, which the repair writes back with its room.
		pdu: CanonicalJsonObject,
	},

	/// The event lacks its room, and nothing names that room.
	Unresolved {
		/// The column holding the event.
		column: Column,

		/// The stored key.
		key: KeyBuf,
	},

	/// The event cannot be read as canonical JSON, so it cannot take a room.
	Unparsable {
		/// The column holding the event.
		column: Column,

		/// The stored key.
		key: KeyBuf,
	},
}

/// Streams each stored event lacking its `room_id`, and each outlier that is not
/// JSON.
///
/// Every timeline row is read, then every outlier. A timeline row that is not
/// JSON at all is skipped, as the checks decoding timeline events report it.
/// Repairing one event changes no other event's finding.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	timeline(services).chain(outliers(services))
}

/// Streams each timeline event lacking its `room_id`.
///
/// Such an event's room comes from the short id leading its key, resolved
/// against `roomid_shortroomid` as it is read before the timeline walk starts.
#[tracing::instrument(level = "trace", skip_all)]
pub fn timeline(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	once(rooms(services))
		.map_ok(move |rooms| {
			services.db["pduid_pdu"]
				.raw_scan()
				.scanned(&services.server)
				.ready_try_filter_map(move |(key, value)| {
					Ok(classify(Column::Timeline, key, value, || timeline_room(&rooms, key)))
				})
		})
		.try_flatten()
}

/// Streams each outlier lacking its `room_id` or not JSON at all.
///
/// Only a create event can lack its room and still name it, through its own id.
#[tracing::instrument(level = "trace", skip_all)]
pub fn outliers(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	services.db["eventid_outlierpdu"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|(key, value)| Ok(classify(Column::Outliers, key, value, || None)))
}

/// Classifies one stored event as [`scan`] does.
///
/// A timeline event's room comes from `timeline_room`, asked only once the event
/// proves to lack one, while an outlier's derives from its own id. A repair reads
/// the row a finding names again and passes it back through here, so detection
/// and repair share one predicate.
pub(crate) fn classify<F>(
	column: Column,
	key: &[u8],
	value: &[u8],
	timeline_room: F,
) -> Option<Finding>
where
	F: FnOnce() -> Option<OwnedRoomId>,
{
	let Ok(probe) = serde_json::from_slice::<Probe>(value) else {
		return matches!(column, Column::Outliers)
			.then(|| Finding::Unparsable { column, key: KeyBuf::from_slice(key) });
	};

	if probe.room_id.is_some() {
		return None;
	}

	let Ok(pdu) = serde_json::from_slice::<CanonicalJsonObject>(value) else {
		return Some(Finding::Unparsable { column, key: KeyBuf::from_slice(key) });
	};

	let room_id = match column {
		| Column::Timeline => timeline_room(),
		| Column::Outliers => outlier_room(key, &pdu),
	};

	let key = KeyBuf::from_slice(key);
	let finding = match room_id {
		| Some(room_id) => Finding::Missing { column, key, room_id, pdu },
		| None => Finding::Unresolved { column, key },
	};

	Some(finding)
}

impl Column {
	/// The name of the column.
	///
	/// The repair indexes the database by it, and a finding renders it.
	#[must_use]
	pub fn name(self) -> &'static str {
		match self {
			| Self::Timeline => "pduid_pdu",
			| Self::Outliers => "eventid_outlierpdu",
		}
	}
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Missing { .. } => "missing-room-id",
			| Self::Unresolved { .. } => "unresolved-room",
			| Self::Unparsable { .. } => "unparsable-event",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Missing { column, key, room_id, .. } => write!(
				f,
				"event {} in {column} has no room_id; it belongs to {room_id}",
				key.escape_ascii()
			),
			| Self::Unresolved { column, key } => write!(
				f,
				"event {} in {column} has no room_id and nothing names its room",
				key.escape_ascii()
			),
			| Self::Unparsable { column, key } => {
				write!(f, "event {} in {column} is not canonical JSON", key.escape_ascii())
			},
		}
	}
}

impl fmt::Display for Column {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.name()) }
}

#[tracing::instrument(level = "debug", skip_all)]
async fn rooms(services: &Services) -> Result<Rooms> {
	services.db["roomid_shortroomid"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|(key, value)| Ok(room(key, value)))
		.try_collect()
		.await
}

fn room(key: &[u8], value: &[u8]) -> Option<(ShortRoomId, OwnedRoomId)> {
	let room_id: &RoomId = from_utf8(key).ok()?.try_into().ok()?;
	let short = short_id(value)?;

	Some((short, room_id.to_owned()))
}

fn timeline_room(rooms: &Rooms, key: &[u8]) -> Option<OwnedRoomId> {
	rooms.get(&short_id(key)?).cloned()
}

fn short_id(bytes: &[u8]) -> Option<ShortRoomId> {
	bytes
		.first_chunk()
		.copied()
		.map(ShortRoomId::from_be_bytes)
}

/// The room of an outlier lacking its `room_id`.
///
/// Only a room version 12 create event omits it, and its room derives from the
/// create event's own id, which keys the outlier.
fn outlier_room(key: &[u8], pdu: &CanonicalJsonObject) -> Option<OwnedRoomId> {
	let is_create = matches!(
		pdu.get("type"),
		Some(CanonicalJsonValue::String(kind)) if kind == "m.room.create"
	);

	is_create.and_then(|| {
		let event_id: &EventId = from_utf8(key).ok()?.try_into().ok()?;

		RoomId::new_v2(event_id.localpart()).ok()
	})
}

#[cfg(test)]
mod tests {
	use super::Probe;

	#[test]
	fn room_id_probe_detects_presence() {
		let with_room_id = br#"{"room_id":"!r:server","type":"m.room.message"}"#;
		let without_room_id = br#"{"type":"m.room.create","sender":"@u:server"}"#;

		let present: Probe = serde_json::from_slice(with_room_id).unwrap();
		let absent: Probe = serde_json::from_slice(without_room_id).unwrap();

		assert!(present.room_id.is_some());
		assert!(absent.room_id.is_none());
	}
}
