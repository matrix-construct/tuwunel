use std::{pin::pin, sync::Arc};

use futures::{TryFutureExt, TryStreamExt};
use ruma::{DeviceId, UserId};
use tuwunel_core::{Result, debug_warn, err, info, warn};
use tuwunel_database::Map;

use crate::{
	Services,
	integrity::token_expiry::{Row, Token, origin, rows, token},
};

/// Stamped once every origin expiry has been adopted.
///
/// Withheld while no OIDC provider is configured and a row is still adoptable,
/// and cleared by the restore before it walks.
pub(super) const ADOPT_MARKER: &str = "adopt_foreign_token_expiry";

/// Stamped once every expiry adopted without a provider has been restored.
///
/// Withheld while a provider is configured, so a provider removed later still
/// gets the restore.
pub(super) const RESTORE_MARKER: &str = "restore_foreign_token_expiry";

/// Counts of what one walk of the origin column did.
///
/// A skipped row and an unreadable one are counted apart because only the
/// second withholds the marker, and withholding it retries the whole pass on the
/// next boot.
#[derive(Default)]
struct Tally {
	applied: usize,
	skipped: usize,
	unreadable: usize,
}

/// What a walk does to the token an origin row names.
///
/// One walk serves every pass over the column, resolving each row through the
/// same lookups and the same provenance guard before the mode decides what, if
/// anything, is written.
#[derive(Clone, Copy)]
enum Mode {
	/// Reports whether the row could be adopted, without writing.
	Probe,

	/// Stamps the origin expiry onto the token.
	Adopt,

	/// Puts a token carrying the stamped expiry back into the foreign shape.
	Restore,
}

/// Whether the restore of adopted expiries has work on this boot.
///
/// The ladder hands the adoption's stamp back only when the restore walks, so it
/// needs to know which case holds before the walk begins.
pub(super) enum Restore {
	/// The database holds no origin column, so nothing was adopted from one.
	Finished,

	/// A provider is configured, against which an adopted expiry refreshes.
	Waiting,

	/// The origin column, whose adopted expiries go back to the foreign shape.
	Due(Arc<Map>),
}

/// Adopts the access-token expiry a foreign database keeps in a column of its
/// own, returning whether the pass is finished with that column.
///
/// That column is keyed by device while this server keeps the expiry alongside
/// the owner in the shared token column, so a migrated session keeps
/// authenticating while its expiry does not.
///
/// Every row names an OAuth session whose client can only recover from the
/// expiry by refreshing against an OIDC provider here, so without one the pass
/// waits, unfinished, rather than stranding sessions. With one it adopts once,
/// since this server writes the same column afterward and a second pass would
/// resurrect a replaced expiry.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn migrate(services: &Services) -> Result<bool> {
	let Some(tokenexpires) = origin(services)? else {
		return Ok(true);
	};

	adopt(services, &tokenexpires).await
}

/// Adopts the expiries an opened origin column records, returning whether the
/// pass is finished with it.
///
/// Without an OIDC provider nothing is written, and the pass is finished only
/// once no row is left that the adoption could still carry.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn adopt(services: &Services, tokenexpires: &Arc<Map>) -> Result<bool> {
	if services.oauth.get_server().is_err() {
		let waiting = probe(services, tokenexpires).await?;

		if waiting {
			info!("Migrated OAuth sessions keep their origin lifetime; no OIDC provider");
		}

		return Ok(!waiting);
	}

	let Tally { applied: adopted, skipped, .. } =
		walk(services, tokenexpires, Mode::Adopt).await?;

	// A skipped row is usually a device this pass correctly left alone, so the
	// summary counts them without implying a loss; a real loss logs per row.
	if adopted > 0 || skipped > 0 {
		info!(%adopted, %skipped, "Adopted token expiry from a foreign database");
	}

	Ok(true)
}

/// Decides whether the restore walks on this boot, opening the origin column if
/// so.
///
/// Before the adoption waited for a provider it stamped every origin expiry,
/// each long past, so a session that has not authenticated since is refused on
/// its next request while its client, with nothing to refresh against, never
/// signs out. With a provider an adopted expiry can be refreshed and stays, so
/// the pass waits rather than finishing, and a provider removed later still gets
/// the restore.
pub(super) fn restore_due(services: &Services) -> Result<Restore> {
	let Some(tokenexpires) = origin(services)? else {
		return Ok(Restore::Finished);
	};

	if services.oauth.get_server().is_ok() {
		return Ok(Restore::Waiting);
	}

	Ok(Restore::Due(tokenexpires))
}

/// Restores the origin lifetime of each session an earlier release adopted with
/// no provider to refresh it against.
///
/// The origin column still names every adopted device with the value stamped,
/// so a token carrying exactly that value goes back to the foreign shape, for
/// the adoption to carry again once a provider is configured.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn restore(services: &Services, tokenexpires: &Arc<Map>) -> Result {
	let Tally { applied: restored, skipped, .. } =
		walk(services, tokenexpires, Mode::Restore).await?;

	if restored > 0 || skipped > 0 {
		info!(
			%restored,
			%skipped,
			"Restored the origin lifetime of adopted OAuth sessions; no OIDC provider"
		);
	}

	Ok(())
}

/// Whether the origin column holds a row the adoption could still carry.
///
/// The first adoptable row answers, so a column of rows this server can never
/// adopt is walked whole while one with work waiting costs a single hit.
async fn probe(services: &Services, tokenexpires: &Arc<Map>) -> Result<bool> {
	let mut adoptable = pin!(rows(services, tokenexpires).try_filter_map(|row| {
		apply_one(services, row, Mode::Probe).map_ok(|adoptable| adoptable.then_some(()))
	}));

	adoptable
		.try_next()
		.map_ok(|hit| hit.is_some())
		.await
}

/// Walks the origin column once, applying the mode to every row.
///
/// One row at a time: each is read before it is rewritten, so a concurrent walk
/// could let two rows naming one token both clear the provenance guard. A
/// cursor error ends the walk rather than being counted, because the status is
/// sticky and the iterator cannot advance past it. An unreadable row fails the
/// walk too, since leaving the marker unstamped is what makes an engine failure
/// recoverable: the pass is idempotent, so the next boot retries it whole.
async fn walk(services: &Services, tokenexpires: &Arc<Map>, mode: Mode) -> Result<Tally> {
	let cork = services.db.cork_and_sync();

	let tally = rows(services, tokenexpires)
		.try_fold(Tally::default(), async |tally, row| {
			let result = apply_one(services, row, mode)
				.inspect_err(|e| warn!(error = %e, "a token expiry could not be read"))
				.await;

			Ok(tally.record(&result))
		})
		.await?;

	drop(cork);

	let unreadable = tally.unreadable;

	unreadable
		.eq(&0)
		.then_some(tally)
		.ok_or_else(|| err!(Database("{unreadable} token expiries could not be read")))
}

impl Tally {
	fn record(mut self, result: &Result<bool>) -> Self {
		let count = match result {
			| Ok(true) => &mut self.applied,
			| Ok(false) => &mut self.skipped,
			| Err(_) => &mut self.unreadable,
		};

		*count = count.saturating_add(1);
		self
	}
}

/// Applies the mode to the token one origin row names, reporting whether the
/// row was acted on.
///
/// A `false` return is a row with nothing to do: one this pass cannot make
/// sense of, a device holding no token here, a token this server does not hold,
/// or a stored value the mode leaves as it is. Only a failed lookup returns an
/// error, because a row that will never decode would otherwise refuse every
/// later boot as well.
async fn apply_one(services: &Services, row: Row, mode: Mode) -> Result<bool> {
	let (user_id, device_id, expires) = match row {
		| Row::Expiry { user_id, device_id, expires, .. } => (user_id, device_id, expires),
		| Row::UnparsableDevice { .. } => {
			warn!("skipping a foreign token expiry whose device could not be read");
			return Ok(false);
		},
		| Row::UnparsableExpiry { user_id, device_id } => {
			warn!(%user_id, %device_id, "skipping a foreign token expiry that could not be read");
			return Ok(false);
		},
	};

	let held = match (token(services, &user_id, &device_id, expires).await?, mode) {
		| (Token::Adoptable(held), Mode::Probe | Mode::Adopt)
		| (Token::Adopted(held), Mode::Restore) => held,
		| (token, _) => {
			skip(&token, &user_id, &device_id);
			return Ok(false);
		},
	};

	let token_owners = &services.db["token_userdeviceid"];
	let (owner, device) = (&*held.user_id, &*held.device_id);

	match mode {
		| Mode::Probe => {},
		| Mode::Adopt => {
			token_owners.raw_put(held.token.as_str(), (owner, device, Some(expires)));
			info!(%user_id, %device_id, "adopted the origin expiry of an access token");
		},
		| Mode::Restore => {
			token_owners.raw_put(held.token.as_str(), (owner, device));
			info!(%user_id, %device_id, "restored the origin lifetime of an access token");
		},
	}

	Ok(true)
}

fn skip(token: &Token, user_id: &UserId, device_id: &DeviceId) {
	match token {
		| Token::Absent => {
			debug_warn!(%user_id, %device_id, "skipping a device holding no access token here");
		},
		| Token::Unparsable => {
			warn!(%user_id, %device_id, "skipping a device whose stored token could not be read");
		},
		| Token::Unheld => {
			debug_warn!(%user_id, %device_id, "skipping an access token this server does not hold");
		},
		| Token::UnparsableValue => {
			warn!(%user_id, %device_id, "skipping a token value that could not be read");
		},
		| Token::Adoptable(_) | Token::Adopted(_) | Token::Kept => {
			debug_warn!(%user_id, %device_id, "skipping a token value this pass leaves as stored");
		},
	}
}
