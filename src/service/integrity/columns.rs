use std::fmt;

use futures::{Stream, StreamExt};
use tuwunel_core::{
	Result,
	utils::{IterStream, TryReadyExt},
};

use super::{Check, Report, ScanExt, tally};
use crate::Services;

pub(super) static CHECK: Check = Check {
	name: "columns",
	about: "Every row of every column reads back and passes its checksum",
	quiescent: false,
	walk: |services, limit| tally(scan(services), limit),
};

pub(super) static UNUSED: Check = Check {
	name: "unused-columns",
	about: "No column on disk is outside this build's schema",
	quiescent: false,
	walk: |services, limit| tally(unused(services), limit),
};

/// A column whose rows could not all be read.
///
/// The walk of a column ends at its first bad block, so this says the column is
/// damaged, not how much of it is.
pub struct Unreadable {
	/// The column.
	pub column: String,

	/// What stopped the walk.
	pub error: String,
}

/// A column on disk that this build's schema does not describe.
///
/// Nothing in this build reads such a column except a migration importing from
/// it, so its bytes are often reclaimable.
pub struct Unused {
	/// The column.
	pub column: String,

	/// Bytes its table files occupy; absent when they could not be measured.
	pub bytes: Option<u64>,
}

/// Streams each column that fails a full read.
///
/// Every row of every column family on the engine is read with its checksum
/// verified, including families this build's schema does not describe. A failed
/// walk is a finding about that column and the next one still runs; only a stop
/// request ends the stream.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Unreadable>> + Send + '_ {
	services
		.db
		.engine
		.column_names()
		.stream()
		.filter_map(async |column| walk(services, column).await.transpose())
}

/// Streams each column on disk that this build's schema does not describe.
///
/// Another server's columns and an older schema's leftovers land here, which is
/// how data nothing reads any more shows up. A foreign column a later migration
/// still wants is listed all the same.
#[tracing::instrument(level = "trace", skip_all)]
pub fn unused(services: &Services) -> impl Stream<Item = Result<Unused>> + Send + '_ {
	services.db.undescribed().stream().map(|column| {
		let bytes = services.db.engine.column_size(&column).ok();

		Ok(Unused { column, bytes })
	})
}

impl Report for Unreadable {
	fn kind(&self) -> &'static str { "unreadable-column" }
}

impl fmt::Display for Unreadable {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "column {} failed a full read: {}", self.column, self.error)
	}
}

impl Report for Unused {
	fn kind(&self) -> &'static str { "unused-column" }
}

impl fmt::Display for Unused {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.bytes {
			| Some(bytes) =>
				write!(f, "column {} is unused and holds {bytes} bytes", self.column),
			| None => write!(f, "column {} is unused", self.column),
		}
	}
}

#[tracing::instrument(level = "debug", skip(services))]
async fn walk(services: &Services, column: String) -> Result<Option<Unreadable>> {
	let Some(map) = services.db.open_cf(&column)? else {
		return Ok(None);
	};

	let walked = map
		.raw_scan()
		.scanned(&services.server)
		.ready_try_for_each(|_| Ok(()))
		.await;

	match walked {
		| Err(error) if error.is_interrupted() => Err(error),
		| Err(error) => Ok(Some(Unreadable { column, error: error.to_string() })),
		| Ok(()) => Ok(None),
	}
}
