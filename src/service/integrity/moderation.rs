//! Suspensions and locks a foreign database records.
//!
//! Some databases keep suspension and lock state in columns of their own, with a
//! richer value. tuwunel keeps only the forensic `when` and `by`, with a row's
//! presence as the flag, and copies each active foreign entry over on every boot.

use std::{borrow::Cow, fmt, sync::Arc};

use futures::{FutureExt, Stream, StreamExt, future::BoxFuture};
use ruma::{MilliSecondsSinceUnixEpoch, UserId};
use serde::Deserialize;
use tuwunel_core::{
	Result,
	arrayvec::ArrayVec,
	result::NotFound,
	utils::{BoolExt, IterStream, TryReadyExt, stream::TryBroadbandExt},
};
use tuwunel_database::{Json, KeyBuf, Map, serialize_val};

use super::{Check, Report, ScanExt, Summary, tally};
use crate::{Services, users::Moderation};

type Sources = ArrayVec<Source, { COLUMNS.len() }>;

pub(super) static CHECK: Check = Check {
	name: "foreign-moderation",
	about: "Every active suspension and lock a foreign column records is copied into tuwunel's \
	        own",
	quiescent: true,
	walk,
};

/// Each foreign column, with the column its entries copy into.
///
/// Both foreign columns hold the same entry shape, so one parser reads either.
const COLUMNS: [(&str, &str); 2] =
	[("userid_suspension", "userid_suspended"), ("userid_lock", "userid_locked")];

/// A foreign entry, of which tuwunel keeps the time and the actor.
///
/// `suspended_by` is an unvalidated string in the source, so it is parsed when
/// the entry is copied, leniently.
#[derive(Deserialize)]
struct Foreign<'a> {
	suspended: bool,
	suspended_at: MilliSecondsSinceUnixEpoch,
	#[serde(borrow)]
	suspended_by: Cow<'a, str>,
}

/// A foreign moderation column the database holds, opened.
///
/// A walk borrows the column it reads, so whoever walks one holds its source for
/// as long as the walk runs.
pub struct Source {
	/// The name of the foreign column.
	pub name: &'static str,

	/// The foreign column.
	pub map: Arc<Map>,

	/// The name of the column its entries copy into.
	pub target: &'static str,
}

/// A foreign entry the copy has yet to carry over, or cannot.
///
/// The copy writes each active entry into its target as tuwunel's own record, and
/// leaves an entry that does not decode.
pub enum Finding {
	/// The entry is active, and its target lacks it or holds another value.
	Uncopied {
		/// The name of the foreign column.
		source: &'static str,

		/// The stored key, the same in both columns.
		key: KeyBuf,

		/// The record the copy writes.
		moderation: Moderation,
	},

	/// The entry does not decode.
	Unparsable {
		/// The name of the foreign column.
		source: &'static str,

		/// The stored key.
		key: KeyBuf,
	},
}

/// One foreign entry as the copy reads it.
///
/// The copy writes every active entry and the check compares each with its
/// target, so both read entries through this.
pub(crate) enum Entry {
	/// The entry is active, read as the record the copy writes.
	Active {
		/// The stored key, the same in both columns.
		key: KeyBuf,

		/// The record the copy writes.
		moderation: Moderation,
	},

	/// The entry does not decode.
	Unparsable {
		/// The stored key.
		key: KeyBuf,

		/// Why it does not decode.
		error: serde_json::Error,
	},
}

/// Opens each foreign moderation column the database holds.
///
/// A column the database lacks is skipped, since its absence is no defect.
pub fn sources(services: &Services) -> impl Iterator<Item = Result<Source>> + Send + '_ {
	COLUMNS.into_iter().filter_map(|(name, target)| {
		services
			.db
			.open_cf(name)
			.transpose()
			.map(|map| map.map(|map| Source { name, map, target }))
	})
}

/// Streams each active entry of a foreign column that its target lacks or holds
/// otherwise, and each entry that does not decode.
///
/// An entry is compared with its target row as the copy would write it, byte for
/// byte. An inactive entry is not reported, as tuwunel has no record for it.
#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(
		source = source.name,
	),
)]
pub fn scan<'a>(
	services: &'a Services,
	source: &'a Source,
) -> impl Stream<Item = Result<Finding>> + Send + 'a {
	let target = &services.db[source.target];

	entries(services, source)
		.broad_and_then(async move |entry| {
			let current = match &entry {
				| Entry::Active { key, .. } => target.get(key).map(NotFound::optional).await?,
				| Entry::Unparsable { .. } => None,
			};

			Ok(finding(source.name, entry, current.as_deref()))
		})
		.ready_try_filter_map(Ok)
}

/// Streams every active entry of a foreign column, and every entry that does not
/// decode.
///
/// The copy writes each active entry whatever its target holds, and reads entries
/// as the check does. An inactive entry is left out, as tuwunel has no record for
/// it.
#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(
		source = source.name,
	),
)]
pub(crate) fn entries<'a>(
	services: &'a Services,
	source: &'a Source,
) -> impl Stream<Item = Result<Entry>> + Send + 'a {
	let fallback = services.globals.server_user.as_ref();

	source
		.map
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(move |(key, value)| Ok(entry(key, value, fallback)))
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Uncopied { .. } => "uncopied-entry",
			| Self::Unparsable { .. } => "unparsable-entry",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Uncopied { source, key, .. } => {
				write!(
					f,
					"active entry {} in {source} has no copy, or another",
					key.escape_ascii()
				)
			},
			| Self::Unparsable { source, key } => {
				write!(f, "entry {} in {source} does not decode", key.escape_ascii())
			},
		}
	}
}

// The opened columns live in this future, since every column's walk borrows one.
fn walk(services: &Services, limit: usize) -> BoxFuture<'_, Result<Summary>> {
	async move {
		let opened: Sources = sources(services).collect::<Result<_>>()?;
		let findings = opened
			.iter()
			.stream()
			.flat_map(|source| scan(services, source));

		tally(findings, limit).await
	}
	.boxed() // type-erasure boundary
}

/// Judges one foreign entry against its target row as [`scan`] does.
///
/// `current` is the target row as it stands. An active entry is uncopied unless
/// that row holds exactly what the copy writes.
pub(super) fn finding(
	source: &'static str,
	entry: Entry,
	current: Option<&[u8]>,
) -> Option<Finding> {
	let (key, moderation) = match entry {
		| Entry::Active { key, moderation } => (key, moderation),
		| Entry::Unparsable { key, .. } => return Some(Finding::Unparsable { source, key }),
	};

	let copied = current.is_some_and(|current| {
		serialize_val(Json(&moderation)).is_ok_and(|expected| expected.as_slice().eq(current))
	});

	copied
		.is_false()
		.then_some(Finding::Uncopied { source, key, moderation })
}

/// Reads one foreign row as the copy and the check do.
///
/// An inactive entry is `None`, and `fallback` is the actor credited when the
/// entry's own does not parse.
pub(super) fn entry(key: &[u8], value: &[u8], fallback: &UserId) -> Option<Entry> {
	let entry = match serde_json::from_slice::<Foreign<'_>>(value) {
		| Ok(entry) => entry,
		| Err(error) => {
			return Some(Entry::Unparsable { key: KeyBuf::from_slice(key), error });
		},
	};

	entry.suspended.then(|| Entry::Active {
		key: KeyBuf::from_slice(key),
		moderation: to_moderation(&entry, fallback),
	})
}

/// Maps a foreign entry to tuwunel's own record.
///
/// An entry whose actor is unparsable is kept all the same, credited to
/// `fallback`: the actor is forensic only, while the row's presence is the flag.
fn to_moderation(entry: &Foreign<'_>, fallback: &UserId) -> Moderation {
	Moderation {
		when: entry.suspended_at,
		by: UserId::parse(&*entry.suspended_by).unwrap_or_else(|_| fallback.to_owned()),
	}
}
