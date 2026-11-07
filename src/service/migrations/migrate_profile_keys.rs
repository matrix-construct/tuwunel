use std::sync::Arc;

use ruma::{UserId, profile::ProfileFieldName};
use tuwunel_core::{
	Result, info,
	result::NotFound,
	utils::{
		ReadyExt, TryReadyExt,
		stream::{TryBroadbandExt, TryExpect},
	},
	warn,
};
use tuwunel_database::{Json, Map};

use super::tally::{Outcome, Tally};
use crate::{
	Services,
	integrity::useridprofilekey_value::{Finding, classify, scan},
};

/// Relocates the per-user displayname and avatar_url out of their dedicated
/// columns into the unified useridprofilekey_value store keyed by MSC4133 field
/// name, where the profile service now reads them.
///
/// The dedicated columns are left intact, so an older binary opening the same
/// database still resolves. The relocation has no check, as those columns go
/// stale, but the values already in the store are then repaired from theirs.
pub(super) async fn migrate_profile_keys(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();

	let userid_displayname = db["userid_displayname"].clone();
	let userid_avatarurl = db["userid_avatarurl"].clone();
	let userid_blurhash = db["userid_blurhash"].clone();
	let useridprofilekey_value = db["useridprofilekey_value"].clone();

	warn!(
		"Relocating displaynames, avatar_urls and blurhashes into the unified profile-key store"
	);

	let displaynames = userid_displayname
		.stream()
		.expect_ok()
		.ready_fold(0_usize, |count, (user_id, displayname): (&UserId, &str)| {
			let key = (user_id, ProfileFieldName::DisplayName.as_str());
			let value = displayname.to_owned();

			useridprofilekey_value.put(key, Json(value));

			count.saturating_add(1)
		})
		.await;

	let avatar_urls = userid_avatarurl
		.stream()
		.expect_ok()
		.ready_fold(0_usize, |count, (user_id, avatar_url): (&UserId, &str)| {
			let key = (user_id, ProfileFieldName::AvatarUrl.as_str());
			let value = avatar_url.to_owned();

			useridprofilekey_value.put(key, Json(value));

			count.saturating_add(1)
		})
		.await;

	let blurhashes = userid_blurhash
		.stream()
		.expect_ok()
		.ready_fold(0_usize, |count, (user_id, blurhash): (&UserId, &str)| {
			let key = (user_id, "xyz.amorgan.blurhash");
			let value = blurhash.to_owned();

			useridprofilekey_value.put(key, Json(value));

			count.saturating_add(1)
		})
		.await;

	// Each finding names its own row, so the repairs cannot interfere.
	let tally = scan(services)
		.broad_and_then(async |finding| repair(&useridprofilekey_value, &finding).await)
		.ready_try_fold(Tally::default(), |tally, outcome| Ok(tally.record(outcome)))
		.await?;

	drop(cork);
	info!(
		%displaynames,
		%avatar_urls,
		%blurhashes,
		fixed_strings = %tally.repaired,
		removed = %tally.removed,
		stale = %tally.stale,
		"Relocated profile keys into useridprofilekey_value"
	);

	useridprofilekey_value.sort()
}

/// Re-encodes or removes the value a finding names, as it stands now.
///
/// The row is read again and classified as the check does, so the text written is
/// the current text. A row removed since the scan, or no longer needing repair, is
/// counted stale.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn repair(
	useridprofilekey_value: &Arc<Map>,
	finding: &Finding,
) -> Result<Outcome> {
	let (Finding::Bare { key, .. } | Finding::Opaque { key, .. }) = finding;
	let current = useridprofilekey_value.get(key).await.optional()?;
	let outcome = match current
		.as_deref()
		.and_then(|value| classify(key, value))
	{
		| None => Outcome::Stale,
		| Some(Finding::Opaque { key, value }) => {
			warn!(?key, ?value, "Non-UTF8 data in profile value");
			useridprofilekey_value.remove(&key);

			Outcome::Removed
		},
		| Some(Finding::Bare { key, text }) => {
			useridprofilekey_value.raw_put(&key, Json(text.as_str()));

			Outcome::Repaired
		},
	};

	Ok(outcome)
}
