use futures::TryStreamExt;
use tuwunel_core::{
	Result, debug_warn, err,
	utils::{IterStream, TryReadyExt},
	warn,
};
use tuwunel_database::{Json, Map};

use super::tally::{Outcome, Tally};
use crate::{
	Services,
	integrity::moderation::{Entry, Source, entries, sources},
};

/// Copies foreign suspension and lock columns into tuwunel's own.
///
/// Runs every boot and is non-destructive: the source columns are read, never
/// removed, so a database also opened by its origin server keeps working. A
/// source column the database lacks copies nothing.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn migrate(services: &Services) -> Result {
	sources(services)
		.stream()
		.try_for_each(async |source| copy(services, &source).await)
		.await
}

/// Copies every active entry of one foreign column into its target.
///
/// Each entry is written whatever the target holds, so the copy reads nothing
/// but the foreign column; the moderation check verifies the result apart. An
/// entry that does not decode is skipped, and logged with the totals.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn copy(services: &Services, source: &Source) -> Result {
	let db = &services.db;
	let target = &db[source.target];
	let cork = db.cork_and_sync();

	let tally = entries(services, source)
		.ready_try_fold(Tally::default(), |tally, entry| Ok(tally.record(write(target, entry))))
		.await?;

	drop(cork);

	if tally.skipped > 0 {
		warn!(
			copied = %tally.repaired,
			skipped = %tally.skipped,
			source = %source.name,
			"Imported moderation entries; some skipped"
		);
	}

	Ok(())
}

fn write(target: &Map, entry: Entry) -> Outcome {
	match entry {
		| Entry::Active { key, moderation } => {
			target.raw_put(&key, Json(moderation));

			Outcome::Repaired
		},
		| Entry::Unparsable { error, .. } => {
			let error = err!(Database("moderation entry is not JSON: {error}"));

			debug_warn!(%error, "skipping unreadable moderation entry");

			Outcome::Skipped
		},
	}
}
