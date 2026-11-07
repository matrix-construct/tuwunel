//! The relation index `relatesto_typed`.
//!
//! An edit or a reference is indexed under its parent, so a parent's edits and
//! references are found without loading its other children. A row holds the
//! child's short event id. The index derives from `pduid_pdu`, and its rebuild
//! clears it before writing it again.

use std::{fmt, sync::Arc};

use futures::{Stream, StreamExt};
use ruma::OwnedEventId;
use tuwunel_core::{
	Error, Result,
	result::NotFound,
	utils::{BoolExt, TryReadyExt, stream::TryBroadbandExt},
};
use tuwunel_database::{KeyBuf, KeyVal, Map};
use tuwunel_matrix::{Pdu, PduId, RawPduId};

use super::{Check, Report, ScanExt, tally};
use crate::{
	Services,
	rooms::pdu_metadata::typed_relations::{Derived, Key, key_child},
};

pub(super) static CHECK: Check = Check {
	name: "relation-index",
	about: "relatesto_typed holds every row its rebuild derives from the timeline, and no other",
	quiescent: true,
	walk: |services, limit| tally(scan(services), limit),
};

/// A `relatesto_typed` row the timeline disagrees with, or an event it cannot
/// read.
///
/// The rebuild writes every row the timeline derives and removes every other, so
/// it repairs all but an unparsable event, an unreadable parent mapping or a
/// malformed short id. A row whose child is absent is residue a history purge
/// leaves on a healthy server, not damage.
pub enum Finding {
	/// An edit or a reference has no index row.
	Missing {
		/// The key the row belongs under.
		key: Key,

		/// The edit or reference.
		child: OwnedEventId,
	},

	/// The row of an edit or a reference holds another event's short id.
	Mismatched {
		/// The row's key.
		key: Key,

		/// The edit or reference.
		child: OwnedEventId,
	},

	/// The child's `eventid_shorteventid` value is not a short id, so whether its
	/// row holds the right one is unknown.
	MalformedShort {
		/// The row's key.
		key: Key,

		/// The edit or reference.
		child: OwnedEventId,
	},

	/// The row names a child with no event behind it, as a history purge leaves.
	AbsentChild {
		/// The row's key.
		key: Key,
	},

	/// The row names a stored child that no longer derives it.
	Stale {
		/// The row's key.
		key: Key,
	},

	/// The row's key has a length the index never writes.
	UnparsableKey {
		/// The stored key.
		key: KeyBuf,
	},

	/// An event row does not decode as an event, so its relation is unknown.
	UnparsableEvent {
		/// The event row's key.
		pdu_id: RawPduId,
	},

	/// An edit or a reference names a parent whose `eventid_pduid` value is not a
	/// PDU id, so its row is unknown.
	UnreadableParent {
		/// The edit or reference.
		child: OwnedEventId,

		/// The parent it names.
		parent: OwnedEventId,
	},
}

/// One row the index should hold.
///
/// The rebuild writes the child's short event id under the key, creating the
/// short id when the child has none.
pub struct Row {
	/// The row's key, naming the parent and the child.
	pub key: Key,

	/// The child, whose short event id the row holds.
	pub child: OwnedEventId,
}

/// What one event row gives the index.
///
/// An event that does not decode, or names a parent whose mapping is unreadable,
/// gives no row to write, but the check reports it. A parent lookup that fails
/// gives no row either: the check returns its error, while the rebuild skips the
/// relation and counts it.
pub(crate) enum Expected {
	/// The event's row.
	Row(Row),

	/// The event row does not decode as an event.
	Unparsable(RawPduId),

	/// The event's parent maps to a value that is not a PDU id.
	UnreadableParent {
		/// The edit or reference.
		child: OwnedEventId,

		/// The parent it names.
		parent: OwnedEventId,
	},

	/// The lookup of the event's parent failed other than as absent.
	Unresolved {
		/// The event row's key.
		pdu_id: RawPduId,

		/// What the lookup returned.
		error: Error,
	},
}

// The event is `None` when the row's value does not decode.
type Source = (RawPduId, Option<Pdu>);

/// Streams each row the index lacks or should not hold, and each event that
/// cannot be read.
///
/// The timeline's rows are read by key against the index, then the index is
/// walked for rows no stored event derives, since the two sort differently. An
/// event row whose key is not a PDU id is skipped, as the timestamp index check
/// reports it.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	let index = &services.db["relatesto_typed"];

	expected(services)
		.broad_and_then(async |expected| verify(services, index, expected).await)
		.chain(stale(services))
		.ready_try_filter_map(Ok)
}

/// Streams the row each edit and reference gives the index.
///
/// The check compares these with the index and the rebuild writes every one, so
/// the two agree on what the index should hold.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn expected(services: &Services) -> impl Stream<Item = Result<Expected>> + Send + '_ {
	services.db["pduid_pdu"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|row| Ok(source(row)))
		.broad_and_then(async |source| derive(services, source).await)
		.ready_try_filter_map(Ok)
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Missing { .. } => "missing-row",
			| Self::Mismatched { .. } => "mismatched-child",
			| Self::MalformedShort { .. } => "malformed-short-id",
			| Self::AbsentChild { .. } => "absent-child",
			| Self::Stale { .. } => "stale-row",
			| Self::UnparsableKey { .. } => "unparsable-key",
			| Self::UnparsableEvent { .. } => "unparsable-event",
			| Self::UnreadableParent { .. } => "unreadable-parent",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Missing { child, .. } => write!(f, "relation {child} has no index row"),
			| Self::Mismatched { child, .. } => {
				write!(f, "index row of relation {child} holds another short id")
			},
			| Self::MalformedShort { child, .. } => {
				write!(f, "relation {child} has a short event id that is not eight bytes")
			},
			| Self::AbsentChild { key } => write!(
				f,
				"index row names a child with no event behind it, as a history purge leaves; \
				 the rebuild drops it: {}",
				key.escape_ascii()
			),
			| Self::Stale { key } => {
				write!(f, "index row no longer derives from its child: {}", key.escape_ascii())
			},
			| Self::UnparsableKey { key } => {
				write!(f, "index key has the wrong length: {}", key.escape_ascii())
			},
			| Self::UnparsableEvent { pdu_id } => {
				write!(f, "event {pdu_id:?} does not decode as an event")
			},
			| Self::UnreadableParent { child, parent } => {
				write!(f, "relation {child} names {parent}, whose PDU id cannot be read")
			},
		}
	}
}

fn source((key, value): KeyVal<'_>) -> Option<Source> {
	let pdu_id = RawPduId::try_from_bytes(key)?;

	Some((pdu_id, serde_json::from_slice(value).ok()))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn derive(services: &Services, (pdu_id, pdu): Source) -> Result<Option<Expected>> {
	let Some(pdu) = pdu else {
		return Ok(Some(Expected::Unparsable(pdu_id)));
	};

	let key = services
		.pdu_metadata
		.typed_relation_key(pdu_id.into(), &pdu)
		.await;

	let expected = match key {
		| Ok(Derived::Unindexed) => None,
		| Ok(Derived::Key(key)) => Some(Expected::Row(Row { key, child: pdu.event_id })),
		| Ok(Derived::UnreadableParent(parent)) =>
			Some(Expected::UnreadableParent { child: pdu.event_id, parent }),
		| Err(error) => Some(Expected::Unresolved { pdu_id, error }),
	};

	Ok(expected)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn verify(
	services: &Services,
	index: &Arc<Map>,
	expected: Expected,
) -> Result<Option<Finding>> {
	let Row { key, child } = match expected {
		| Expected::Row(row) => row,
		| Expected::Unparsable(pdu_id) => {
			return Ok(Some(Finding::UnparsableEvent { pdu_id }));
		},
		| Expected::UnreadableParent { child, parent } => {
			return Ok(Some(Finding::UnreadableParent { child, parent }));
		},
		| Expected::Unresolved { error, .. } => return Err(error),
	};

	let Some(stored) = index
		.get(key.as_slice())
		.await
		.optional()?
		.map(|short| <[u8; 8]>::try_from(&*short).ok())
	else {
		return Ok(Some(Finding::Missing { key, child }));
	};

	let short = services.db["eventid_shorteventid"]
		.get(&child)
		.await
		.optional()?
		.map(|short| <[u8; 8]>::try_from(&*short));

	let short = match short {
		| Some(Err(_)) => return Ok(Some(Finding::MalformedShort { key, child })),
		| short => short.and_then(Result::ok),
	};

	let mismatched = stored
		.zip(short)
		.is_none_or(|(stored, short)| stored != short);

	let finding = mismatched.then_some(Finding::Mismatched { key, child });

	Ok(finding)
}

/// Streams each index row no stored event derives, and each unparsable key.
///
/// A row's child is read by key and derived again, and a row it does not derive
/// is one the rebuild drops.
fn stale(services: &Services) -> impl Stream<Item = Result<Option<Finding>>> + Send + '_ {
	let pdus = &services.db["pduid_pdu"];

	services.db["relatesto_typed"]
		.raw_scan()
		.scanned(&services.server)
		.ready_and_then(|(key, _)| Ok(stored(key).ok_or_else(|| KeyBuf::from_slice(key))))
		.broad_and_then(async |stored| match stored {
			| Err(key) => Ok(Some(Finding::UnparsableKey { key })),
			| Ok((key, child)) => derives(services, pdus, key, child).await,
		})
}

fn stored(key: &[u8]) -> Option<(Key, PduId)> {
	let child = key_child(key)?;

	Some((Key::try_from(key).ok()?, child))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn derives(
	services: &Services,
	pdus: &Arc<Map>,
	key: Key,
	child: PduId,
) -> Result<Option<Finding>> {
	let pdu_id: RawPduId = child.into();
	let Some(pdu) = pdus
		.get(&pdu_id)
		.await
		.optional()?
		.map(|pdu| serde_json::from_slice::<Pdu>(&pdu))
	else {
		return Ok(Some(Finding::AbsentChild { key }));
	};

	let derives = match pdu {
		| Err(_) => false,
		| Ok(pdu) => matches!(
			services
				.pdu_metadata
				.typed_relation_key(child, &pdu)
				.await?,
			Derived::Key(derived) if derived == key
		),
	};

	let finding = derives
		.is_false()
		.then_some(Finding::Stale { key });

	Ok(finding)
}
