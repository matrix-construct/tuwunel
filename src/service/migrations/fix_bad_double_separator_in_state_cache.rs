use std::sync::Arc;

use futures::TryStreamExt;
use tuwunel_core::{Result, debug_warn, info, result::NotFound, utils::TryReadyExt, warn};
use tuwunel_database::Map;

use super::tally::{Outcome, Tally};
use crate::{
	Services,
	integrity::roomuserid_joined::{Finding, classify, scan},
};

pub(super) async fn fix_bad_double_separator_in_state_cache(services: &Services) -> Result {
	warn!("Fixing bad double separator in state_cache roomuserid_joined");

	let db = &services.db;
	let roomuserid_joined = &db["roomuserid_joined"];
	let cork = db.cork_and_sync();

	// Serial, because closing up a tripled separator writes another doubled key.
	let tally = scan(services)
		.and_then(async |finding| repair(roomuserid_joined, &finding).await)
		.ready_try_fold(Tally::default(), |tally, outcome| Ok(tally.record(outcome)))
		.await?;

	drop(cork);
	info!(
		fixed = %tally.repaired,
		skipped = %tally.skipped,
		stale = %tally.stale,
		"Finished fixing"
	);

	roomuserid_joined.sort()
}

/// Moves the row a finding names under its corrected key, as it stands now.
///
/// The row is read again and classified as the check does, so the value moved is
/// the current one and a row changed or removed since the scan is counted stale.
#[tracing::instrument(level = "debug", skip_all)]
async fn repair(roomuserid_joined: &Arc<Map>, finding: &Finding) -> Result<Outcome> {
	let key = match finding {
		| Finding::Doubled { key, .. } => key,
		| Finding::Unseparated { key } => {
			debug_warn!(?key, "roomuserid_joined key has no 0xFF separator; skipping");

			return Ok(Outcome::Skipped);
		},
	};

	let current = roomuserid_joined.get(key).await.optional()?;
	let outcome = match current
		.as_deref()
		.and_then(|value| classify(key, value))
	{
		| None | Some(Finding::Unseparated { .. }) => Outcome::Stale,
		| Some(Finding::Doubled { key, single, value }) => {
			debug_warn!(?key, "Found bad key");
			roomuserid_joined.remove(&key);

			debug_warn!(?single, "Fixed key");
			roomuserid_joined.insert(&single, value);

			Outcome::Repaired
		},
	};

	Ok(outcome)
}
