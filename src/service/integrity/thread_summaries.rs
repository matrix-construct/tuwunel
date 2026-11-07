//! The thread summaries stored on thread roots.
//!
//! A root's `unsigned` holds its thread's reply count and latest reply, kept
//! current as replies arrive and are redacted. The roots are the ones
//! `threadid_userids` lists.

use std::{fmt, sync::Arc};

use futures::{Stream, TryStreamExt};
use ruma::{CanonicalJsonObject, UInt};
use tuwunel_core::{
	Result,
	itertools::Either::{Left, Right},
	result::NotFound,
	utils::TryReadyExt,
};
use tuwunel_database::{KeyBuf, Map};
use tuwunel_matrix::RawPduId;

use super::{Check, Report, ScanExt, leading_json, tally};
use crate::{Services, rooms::threads::Recount};

pub(super) static CHECK: Check = Check {
	name: "thread-summaries",
	about: "Every listed thread root's summary agrees with a recount of its replies",
	quiescent: true,
	walk: |services, limit| tally(scan(services), limit),
};

/// A thread root whose stored summary disagrees with its replies, or cannot be
/// read.
///
/// The summary rebuild rewrites a disagreeing summary, and leaves the rest. A
/// root whose relation index holds a damaged key, or whose latest reply has a
/// damaged timeline mapping, is reported for that alone, since its recount is
/// incomplete.
pub enum Finding {
	/// The summary's count, or its latest reply, disagrees with the replies.
	Stale {
		/// The root.
		root_id: RawPduId,

		/// The count the summary holds.
		stored: Option<UInt>,

		/// The count the replies give.
		counted: Option<UInt>,

		/// Whether the summary's latest reply is gone or redacted.
		latest: bool,
	},

	/// The root's relation index holds keys too short to name a reply.
	MalformedRelation {
		/// The root.
		root_id: RawPduId,

		/// How many such keys the recount skipped.
		malformed: usize,
	},

	/// The summary's latest reply has a timeline mapping that is not a PDU id.
	MalformedLatest {
		/// The root.
		root_id: RawPduId,
	},

	/// The root's row does not decode.
	Unreadable {
		/// The root.
		root_id: RawPduId,
	},

	/// A `threadid_userids` key is not a PDU id.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},
}

/// Streams each listed thread root whose summary disagrees with a recount.
///
/// A root no longer stored, or holding no summary, is skipped as the rebuild skips
/// it. The rebuild re-reads each root under its room lock and decides through the
/// same recount, so the two agree. A reply or latest reply whose row cannot be
/// read ends the stream with its error, while a latest reply's mapping that does
/// not decode is a finding.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	let pdus = &services.db["pduid_pdu"];

	// The recount already fans out per root, so a concurrent stage here nests fan-outs.
	services.db["threadid_userids"]
		.raw_scan()
		.scanned(&services.server)
		.ready_and_then(|(key, _)| {
			Ok(RawPduId::try_from_bytes(key).ok_or_else(|| KeyBuf::from_slice(key)))
		})
		.and_then(async |root_id| match root_id {
			| Ok(root_id) => verify(services, pdus, root_id).await,
			| Err(key) => Ok(Some(Finding::Unparsable { key })),
		})
		.ready_try_filter_map(Ok)
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Stale { .. } => "stale-summary",
			| Self::MalformedRelation { .. } => "malformed-relation",
			| Self::MalformedLatest { .. } => "malformed-mapping",
			| Self::Unreadable { .. } => "unreadable-root",
			| Self::Unparsable { .. } => "unparsable-key",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Unreadable { root_id } => {
				write!(f, "thread root {root_id:?} does not decode")
			},
			| Self::Unparsable { key } => {
				write!(f, "thread key is not a PDU id: {}", key.escape_ascii())
			},
			| Self::MalformedLatest { root_id } => write!(
				f,
				"latest reply of thread root {root_id:?} has a timeline mapping that is not a \
				 PDU id"
			),
			| Self::MalformedRelation { root_id, malformed } => {
				write!(
					f,
					"thread root {root_id:?} has {malformed} relation keys too short to read"
				)
			},
			| Self::Stale { root_id, stored, counted, latest } => {
				let shown = |count: &Option<UInt>| count.map_or(Left("none"), Right);

				write!(
					f,
					"summary of thread root {root_id:?} counts {} where its replies give {}, \
					 latest reply gone or redacted: {latest}",
					shown(stored),
					shown(counted),
				)
			},
		}
	}
}

#[tracing::instrument(level = "debug", skip_all)]
async fn verify(
	services: &Services,
	pdus: &Arc<Map>,
	root_id: RawPduId,
) -> Result<Option<Finding>> {
	let Some(root) = pdus
		.get(&root_id)
		.await
		.optional()?
		.map(|root| leading_json::<CanonicalJsonObject>(&root))
	else {
		return Ok(None);
	};

	let Ok(root) = root else {
		return Ok(Some(Finding::Unreadable { root_id }));
	};

	let recount = services
		.threads
		.recount_summary(root_id, &root)
		.await?;

	let finding = recount
		.as_ref()
		.and_then(|recount| finding(root_id, recount));

	Ok(finding)
}

fn finding(root_id: RawPduId, recount: &Recount) -> Option<Finding> {
	let &Recount {
		stored,
		counted,
		stale_latest,
		malformed,
		malformed_latest,
		..
	} = recount;

	match malformed {
		| 0 if malformed_latest => Some(Finding::MalformedLatest { root_id }),
		| 0 if recount.changed() => Some(Finding::Stale {
			root_id,
			stored,
			counted,
			latest: stale_latest,
		}),
		| 0 => None,
		| malformed => Some(Finding::MalformedRelation { root_id, malformed }),
	}
}
