use std::sync::Arc;

use tuwunel_core::{
	Result, debug, info,
	result::NotFound,
	utils::{TryReadyExt, stream::TryBroadbandExt},
	warn,
};
use tuwunel_database::Map;

use super::tally::{Outcome, Tally};
use crate::{
	Services,
	integrity::userid_password::{Finding, classify, scan},
	users::PASSWORD_SENTINEL,
};

/// Rewrites each hashed sentinel back to the sentinel.
///
/// With no identity provider configured the pass waits and reports unfinished, so
/// a provider added later still gets it.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn fix_hashed_sentinel_passwords(services: &Services) -> Result<bool> {
	if services.config.identity_provider.is_empty() {
		debug!("Skipping sentinel password migration since no SSO IdP configured.");
		return Ok(false);
	}

	let db = &services.db;
	let cork = db.cork_and_sync();
	let userid_password = &db["userid_password"];

	warn!(sentinel = %PASSWORD_SENTINEL, "Fixing occurrences of hashed sentinel passwords");

	// Each finding names its own row, so the repairs cannot interfere.
	let tally = scan(services)
		.broad_and_then(async |finding| repair(userid_password, &finding).await)
		.ready_try_fold(Tally::default(), |tally, outcome| Ok(tally.record(outcome)))
		.await?;

	drop(cork);
	info!(
		bad = %tally.repaired,
		stale = %tally.stale,
		unparsable = %tally.skipped,
		"Fixed any occurrences of hashed sentinel passwords"
	);

	userid_password.sort()?;

	Ok(true)
}

/// Rewrites the password a finding names, if it still hashes the sentinel.
///
/// The row is read again and classified as the check does, so a row changed or
/// removed since the scan is counted stale and left alone.
#[tracing::instrument(level = "debug", skip_all)]
async fn repair(userid_password: &Arc<Map>, finding: &Finding) -> Result<Outcome> {
	let Finding::Hashed { user_id } = finding else {
		return Ok(Outcome::Skipped);
	};

	let current = userid_password
		.get(user_id.as_str())
		.await
		.optional()?;

	let hashed = current
		.as_deref()
		.and_then(|value| classify(user_id.as_bytes(), value))
		.is_some_and(|finding| matches!(finding, Finding::Hashed { .. }));

	if !hashed {
		return Ok(Outcome::Stale);
	}

	userid_password.insert(user_id.as_str(), PASSWORD_SENTINEL);

	Ok(Outcome::Repaired)
}
