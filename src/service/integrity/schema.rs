use std::fmt;

use futures::{Stream, StreamExt, stream::once};
use tuwunel_core::{
	Result,
	utils::{BoolExt, IterStream, TryReadyExt, u64_from_bytes},
};

use super::{Check, Report, tally};
use crate::{
	Services,
	migrations::{DATABASE_VERSION, MARKERS, marker_present},
};

pub(super) static CHECK: Check = Check {
	name: "schema",
	about: "The schema version matches this build and a fresh database's markers are stamped",
	quiescent: false,
	walk: |services, limit| tally(scan(services), limit),
};

/// A way the database's schema departs from this build's.
///
/// Neither is damage in itself. Each says the rows may not be in the shape this
/// build reads.
#[derive(Clone, Copy)]
pub enum Finding {
	/// The stored schema version is not this build's.
	Version {
		/// The version stored in the database.
		stored: u64,
	},

	/// No schema version is stored.
	Unversioned,

	/// The stored schema version is not eight bytes.
	Malformed,

	/// A migration has not stamped its marker.
	Pending {
		/// The marker of the step, which is also its name.
		step: &'static str,
	},
}

/// Streams each departure of the stored schema from this build's.
///
/// A version mismatch comes first. Between it and the pending steps a reader can
/// tell a database that was never upgraded from one a build abandoned partway,
/// including after a downgrade. A failed read ends the stream, so an unreadable
/// record is never reported as a version.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	let version = once(version(services)).ready_try_filter_map(Ok);

	let pending = MARKERS
		.iter()
		.copied()
		.stream()
		.then(async |step| Ok((step, marker_present(services, step).await?)))
		.ready_try_filter_map(|(step, present)| {
			Ok(present
				.is_false()
				.then_some(Finding::Pending { step }))
		});

	version.chain(pending)
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Version { .. } => "schema-version",
			| Self::Unversioned => "missing-version",
			| Self::Malformed => "malformed-version",
			| Self::Pending { .. } => "pending-migration",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Version { stored } => {
				write!(f, "schema version is {stored}; this build uses {DATABASE_VERSION}")
			},
			| Self::Unversioned => write!(f, "no schema version is stored"),
			| Self::Malformed => write!(f, "the stored schema version does not decode"),
			| Self::Pending { step } => write!(f, "migration {step} is not stamped"),
		}
	}
}

#[tracing::instrument(level = "debug", skip_all)]
async fn version(services: &Services) -> Result<Option<Finding>> {
	match services
		.globals
		.db
		.database_version_record()
		.await
	{
		| Err(error) if error.is_not_found() => Ok(Some(Finding::Unversioned)),
		| Err(error) => Err(error),
		| Ok(record) => Ok(match u64_from_bytes(&record) {
			| Ok(DATABASE_VERSION) => None,
			| Ok(stored) => Some(Finding::Version { stored }),
			| Err(_) => Some(Finding::Malformed),
		}),
	}
}
