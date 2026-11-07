use std::sync::Arc;

use tuwunel_core::{
	Result, info,
	result::NotFound,
	utils::{TryReadyExt, stream::TryBroadbandExt},
	warn,
};
use tuwunel_database::{Map, SEP};

use super::tally::{Outcome, Tally};
use crate::{
	Services,
	integrity::mediaid_user::{Legacy, classify_legacy, legacy},
};

pub(super) async fn upgrade_legacy_mediaid_user(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let mediaid_user = &db["mediaid_user"];

	warn!("Upgrading legacy mediaid_user keys to composite (mxc, user_id) layout");

	// A legacy key has no separator, so each converts to a key no other one does,
	// and the repairs cannot interfere.
	let tally = legacy(services)
		.broad_and_then(async |finding| repair(mediaid_user, &finding).await)
		.ready_try_fold(Tally::default(), |tally, outcome| Ok(tally.record(outcome)))
		.await?;

	drop(cork);
	info!(
		upgraded = %tally.repaired,
		removed_invalid = %tally.removed,
		stale = %tally.stale,
		"Upgraded legacy mediaid_user keys"
	);

	mediaid_user.sort()
}

/// Upgrades or removes the row a finding names, as it stands now.
///
/// The row is read again and classified as the check does, so the uploader comes
/// from the current value. A row removed since the scan, or no longer legacy, is
/// counted stale.
#[tracing::instrument(level = "debug", skip_all)]
async fn repair(mediaid_user: &Arc<Map>, finding: &Legacy) -> Result<Outcome> {
	let (Legacy::Upgradable { key, .. } | Legacy::Unowned { key, .. }) = finding;
	let current = mediaid_user.get(key).await.optional()?;
	let outcome = match current
		.as_deref()
		.and_then(|value| classify_legacy(key, value))
	{
		| None => Outcome::Stale,
		| Some(Legacy::Unowned { key, value }) => {
			warn!(?key, ?value, "Legacy entry has unparsable user_id, removing");

			mediaid_user.remove(&key);

			Outcome::Removed
		},
		| Some(Legacy::Upgradable { key, user_id }) => {
			let upgraded_key = [key.as_slice(), &[SEP], user_id.as_bytes()].concat();

			mediaid_user.insert(&upgraded_key, user_id.as_str());
			mediaid_user.remove(&key);

			Outcome::Repaired
		},
	};

	Ok(outcome)
}
