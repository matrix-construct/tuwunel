//! Redactions a foreign database applied without recording them.
//!
//! Some origin releases keep only a reference to a redaction on the event it
//! redacted, both when redacting and in a rewrite of every stored row. On this
//! server such an event reads as unredacted while its content is already gone.

use std::fmt;

use futures::{Stream, TryStreamExt, stream::once};
use ruma::{OwnedEventId, OwnedRoomId, RoomVersionId, events::TimelineEventType};
use serde::Deserialize;
use tuwunel_core::{
	Result,
	result::NotFound,
	utils::{TryReadyExt, stream::TryBroadbandExt},
};
use tuwunel_database::KeyBuf;
use tuwunel_matrix::{event::Event, pdu::PduEvent, room_version::rules as room_rules};

use super::{Check, Report, ScanExt, per_room, tally};
use crate::{
	Services,
	migrations::marker_present,
	rooms::timeline::{Mapped, Restoration},
};

pub(super) static CHECK: Check = Check {
	name: "unrecorded-redactions",
	about: "Where an origin recording redactions by reference opened the database, every \
	        redaction whose target's content is gone is recorded on the target",
	quiescent: true,
	walk: |services, limit| tally(scan(services), limit),
};

// Stamped by the origin when its rewrite of the stored redactions completes.
const ORIGIN_MARKER: &str = "unembed_unsigned_info";

// Created on every open by an origin release recording redactions by reference.
const ORIGIN_COLUMN: &str = "servernamekeyid_response";

// A room's version, read once for its redactions; none when it has no create event.
type Version = Option<Mapped<RoomVersionId>>;

/// The event type of a stored row, read without the rest of the event.
///
/// The walk reads every timeline row, so only a row this shows to be a
/// redaction pays for the full parse.
#[derive(Deserialize)]
struct Kind {
	#[serde(rename = "type")]
	kind: TimelineEventType,
}

/// A redaction its target does not record although the target's content is gone.
///
/// The repair records each such redaction as a redaction here leaves it. A
/// redaction this check cannot decide is reported apart, and the repair fails on
/// it.
pub enum Finding {
	/// The target's content is in its redacted form, but its row records this
	/// redaction nowhere, or only an earlier one.
	Unrecorded {
		/// The redaction.
		redaction: OwnedEventId,

		/// The event it redacts.
		target: OwnedEventId,
	},

	/// The row is typed a redaction but does not parse as an event.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},

	/// The redaction's room has no version this server reads and supports.
	UnknownRoom {
		/// The redaction.
		redaction: OwnedEventId,

		/// Its room.
		room_id: OwnedRoomId,
	},

	/// A row the decision reads is damaged: one leading from the room's current
	/// state to its create event, or a timeline mapping, the create event's
	/// included.
	MalformedMapping {
		/// The redaction.
		redaction: OwnedEventId,
	},

	/// The target's row is missing or does not decode, the target does not take
	/// the redaction, or the redaction does not convert to canonical JSON.
	Undecidable {
		/// The redaction.
		redaction: OwnedEventId,

		/// The event it redacts.
		target: OwnedEventId,
	},
}

/// A redaction event the timeline stores, or a row typed a redaction that does
/// not parse as one.
///
/// The event is carried as it was parsed, while a row that does not parse keeps
/// its key and the parse error, so the check can name the row and the repair can
/// log why it failed.
pub(crate) type Redaction = Result<PduEvent, Unparsable>;

/// A timeline row typed a redaction that does not parse as an event.
///
/// Its key names the row in a finding, and its error is what the repair logs.
pub(crate) struct Unparsable {
	/// The stored key.
	pub(crate) key: KeyBuf,

	/// Why it does not parse.
	pub(crate) error: serde_json::Error,
}

/// Streams each redaction recording would apply to its target, and each one this
/// check cannot decide.
///
/// Only a database an origin recording redactions by reference has opened is
/// walked, as only there does the repair run. Each redaction is decided as
/// recording it would decide, against its target as it stands, its room's
/// version read once for each run of redactions in one room.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	let redactions = once(records_by_reference(services))
		.map_ok(move |recorded| recorded.then(|| redactions(services)))
		.ready_try_filter_map(Ok)
		.try_flatten();

	per_room(redactions, PduEvent::room_id, move |room_id| version(services, room_id))
		.broad_and_then(async |redaction| classify(services, redaction).await)
		.ready_try_filter_map(Ok)
}

/// Whether the origin ever opened the database with a release that records
/// redactions by reference.
///
/// The stamp lands only when the origin finishes rewriting the stored rows, while
/// its redactions take the new form from the first open, so the column that
/// release creates on every open answers for a rewrite never finished.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn records_by_reference(services: &Services) -> Result<bool> {
	if services.db.open_cf(ORIGIN_COLUMN)?.is_some() {
		return Ok(true);
	}

	marker_present(services, ORIGIN_MARKER).await
}

/// Streams every redaction event the timeline stores.
///
/// The repair attempts each one and the check decides each one, so the two agree
/// on which redactions there are.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn redactions(
	services: &Services,
) -> impl Stream<Item = Result<Redaction>> + Send + '_ {
	services.db["pduid_pdu"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|(key, value)| Ok(redaction(key, value)))
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Unrecorded { .. } => "unrecorded-redaction",
			| Self::Unparsable { .. } => "unparsable-redaction",
			| Self::UnknownRoom { .. } => "unknown-room-version",
			| Self::MalformedMapping { .. } => "malformed-mapping",
			| Self::Undecidable { .. } => "undecidable-redaction",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Unrecorded { redaction, target } => {
				write!(f, "{target} lost its content to {redaction} but does not record it")
			},
			| Self::Unparsable { key } => {
				write!(f, "redaction {} does not parse as an event", key.escape_ascii())
			},
			| Self::UnknownRoom { redaction, room_id } => {
				write!(f, "redaction {redaction} is in {room_id}, whose version is unknown")
			},
			| Self::MalformedMapping { redaction } =>
				write!(f, "a row deciding redaction {redaction} is damaged"),
			| Self::Undecidable { redaction, target } => write!(
				f,
				"whether {redaction} applies to {target} is unknown: a row it needs does not \
				 read"
			),
		}
	}
}

#[tracing::instrument(level = "debug", skip(services))]
async fn version(services: &Services, room_id: OwnedRoomId) -> Result<Version> {
	services
		.state
		.room_version_checked(&room_id)
		.await
		.present()
}

#[tracing::instrument(level = "debug", skip_all)]
async fn classify(
	services: &Services,
	redaction: Result<(PduEvent, Version), Unparsable>,
) -> Result<Option<Finding>> {
	let (redaction, version) = match redaction {
		| Ok(redaction) => redaction,
		| Err(Unparsable { key, .. }) => return Ok(Some(Finding::Unparsable { key })),
	};

	let room_id = redaction.room_id();
	let event_id = || redaction.event_id().to_owned();
	let rules = match version {
		| Some(Mapped::Event(version)) => room_rules(&version).ok(),
		| Some(Mapped::Damaged) => {
			return Ok(Some(Finding::MalformedMapping { redaction: event_id() }));
		},
		| None => None,
	};

	let Some(rules) = rules else {
		let finding = Finding::UnknownRoom {
			redaction: event_id(),
			room_id: room_id.to_owned(),
		};

		return Ok(Some(finding));
	};

	let Some(target) = redaction.redacts_id(&rules) else {
		return Ok(None);
	};

	let restoration = services
		.timeline
		.redaction_restoration(&target, &redaction, &rules.redaction)
		.await?;

	let finding = match restoration {
		| Restoration::Due => Some(Finding::Unrecorded { redaction: event_id(), target }),
		| Restoration::Moot => None,
		| Restoration::Malformed => Some(Finding::MalformedMapping { redaction: event_id() }),
		| Restoration::Unreadable => Some(Finding::Undecidable { redaction: event_id(), target }),
	};

	Ok(finding)
}

/// The redaction event a timeline row holds.
///
/// Any other row is `None`, as is one whose type cannot be read, since nothing
/// can be restored from it.
fn redaction(key: &[u8], value: &[u8]) -> Option<Redaction> {
	serde_json::from_slice(value)
		.is_ok_and(|probe: Kind| probe.kind.eq(&TimelineEventType::RoomRedaction))
		.then(|| {
			serde_json::from_slice(value)
				.map_err(|error| Unparsable { key: KeyBuf::from_slice(key), error })
		})
}
