use std::{pin::pin, sync::Arc};

use futures::{TryFutureExt, TryStreamExt};
use ruma::{DeviceId, UserId};
use tuwunel_core::{Result, debug_warn, err, info, result::NotFound, warn};
use tuwunel_database::{KeyVal, Map, deserialize_from_slice, serialize_key};

use crate::Services;

/// Stamped once every origin expiry has been adopted.
pub(super) const ADOPT_MARKER: &str = "adopt_foreign_token_expiry";

/// Stamped once every expiry adopted without a provider has been restored.
pub(super) const RESTORE_MARKER: &str = "restore_foreign_token_expiry";

const ORIGIN_COLUMN: &str = "userdeviceid_tokenexpires";

type Device<'a> = (&'a UserId, &'a DeviceId);

type TokenValue<'a> = (&'a UserId, &'a DeviceId, Option<u64>);

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
pub(super) async fn migrate_token_expiry(services: &Services) -> Result<bool> {
	let Some(tokenexpires) = services.db.open_cf(ORIGIN_COLUMN)? else {
		return Ok(true);
	};

	if services.oauth.get_server().is_err() {
		let waiting = probe(services, &tokenexpires).await?;

		if waiting {
			info!("Migrated OAuth sessions keep their origin lifetime; no OIDC provider");
		}

		return Ok(!waiting);
	}

	let Tally { applied: adopted, skipped, .. } =
		walk(services, &tokenexpires, Mode::Adopt).await?;

	// A skipped row is usually a device this pass correctly left alone, so the
	// summary counts them without implying a loss; a real loss logs per row.
	if adopted > 0 || skipped > 0 {
		info!(%adopted, %skipped, "Adopted token expiry from a foreign database");
	}

	Ok(true)
}

/// Restores the origin lifetime of a session an earlier release adopted with
/// no provider to refresh it against, returning whether the pass is finished.
///
/// Before the adoption waited for a provider it stamped every origin expiry,
/// each long past, so a session that has not authenticated since is refused and
/// removed on its next request while its client, with nothing to refresh
/// against, never signs out. The origin column still names every adopted device
/// with the value stamped, so a token carrying exactly that value goes back to
/// the foreign shape, and the adoption marker is cleared so the gated pass owns
/// the column again from this boot on. With a provider an adopted expiry can be
/// refreshed and stays, so the pass waits rather than finishing, and a provider
/// removed later still gets the restore.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn restore_token_expiry(services: &Services) -> Result<bool> {
	let Some(tokenexpires) = services.db.open_cf(ORIGIN_COLUMN)? else {
		return Ok(true);
	};

	if services.oauth.get_server().is_ok() {
		return Ok(false);
	}

	// Handed back before the walk, so however it ends the restored rows are the
	// adoption's to own.
	services.db["global"].remove(ADOPT_MARKER);

	let Tally { applied: restored, skipped, .. } =
		walk(services, &tokenexpires, Mode::Restore).await?;

	if restored > 0 || skipped > 0 {
		info!(
			%restored,
			%skipped,
			"Restored the origin lifetime of adopted OAuth sessions; no OIDC provider"
		);
	}

	Ok(true)
}

/// Whether the origin column holds a row the adoption could still carry.
///
/// The first adoptable row answers, so a column of rows this server can never
/// adopt is walked whole while one with work waiting costs a single hit.
async fn probe(services: &Services, tokenexpires: &Arc<Map>) -> Result<bool> {
	let (device_tokens, token_owners) = columns(services);
	let mut adoptable = pin!(tokenexpires.raw_stream().try_filter_map(|row| {
		apply_one(device_tokens, token_owners, row, Mode::Probe)
			.map_ok(|adoptable| adoptable.then_some(()))
	}));

	adoptable
		.try_next()
		.map_ok(|hit| hit.is_some())
		.await
}

fn columns(services: &Services) -> (&Arc<Map>, &Arc<Map>) {
	(&services.db["userdeviceid_token"], &services.db["token_userdeviceid"])
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
	let (device_tokens, token_owners) = columns(services);
	let cork = services.db.cork_and_sync();

	let tally = tokenexpires
		.raw_stream()
		.try_fold(Tally::default(), async |tally, row| {
			Ok(tally.record(apply_one(device_tokens, token_owners, row, mode).await))
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
	fn record(mut self, result: Result<bool>) -> Self {
		match result {
			| Ok(true) => self.applied = self.applied.saturating_add(1),
			| Ok(false) => self.skipped = self.skipped.saturating_add(1),
			| Err(e) => {
				warn!(error = %e, "a token expiry could not be read");
				self.unreadable = self.unreadable.saturating_add(1);
			},
		}

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
async fn apply_one(
	device_tokens: &Arc<Map>,
	token_owners: &Arc<Map>,
	(key, value): KeyVal<'_>,
	mode: Mode,
) -> Result<bool> {
	let Ok((user_id, device_id)) = deserialize_from_slice::<Device<'_>>(key) else {
		warn!("skipping a foreign token expiry whose device could not be read");
		return Ok(false);
	};

	let Ok(expires) = deserialize_from_slice::<u64>(value) else {
		warn!(%user_id, %device_id, "skipping a foreign token expiry that could not be read");
		return Ok(false);
	};

	let Some(token_raw) = device_tokens
		.qry(&(user_id, device_id))
		.await
		.optional()?
	else {
		debug_warn!(%user_id, %device_id, "skipping a device holding no access token here");
		return Ok(false);
	};

	let Ok(token) = deserialize_from_slice::<&str>(&token_raw) else {
		warn!(%user_id, %device_id, "skipping a device whose stored token could not be read");
		return Ok(false);
	};

	let Some(stored) = token_owners.get(token).await.optional()? else {
		debug_warn!(%user_id, %device_id, "skipping an access token this server does not hold");
		return Ok(false);
	};

	let candidate = match mode {
		| Mode::Probe | Mode::Adopt => adoptable(&stored),
		| Mode::Restore => adopted(&stored, expires),
	};

	let Ok(candidate) = candidate else {
		warn!(%user_id, %device_id, "skipping a token value that could not be read");
		return Ok(false);
	};

	let Some((owner, device)) = candidate else {
		debug_warn!(%user_id, %device_id, "skipping a token value this pass leaves as stored");
		return Ok(false);
	};

	match mode {
		| Mode::Probe => {},
		| Mode::Adopt => {
			token_owners.raw_put(token, (owner, device, Some(expires)));
			info!(%user_id, %device_id, "adopted the origin expiry of an access token");
		},
		| Mode::Restore => {
			token_owners.raw_put(token, (owner, device));
			info!(%user_id, %device_id, "restored the origin lifetime of an access token");
		},
	}

	Ok(true)
}

/// Reads a stored token value, yielding its owner only when the row is one this
/// pass may annotate.
///
/// A foreign row omits the trailing expiry field and reads as `None`. This
/// server always writes that field, even empty, so every row it wrote encodes
/// shorter than it is stored. The length comparison is what skips a token issued
/// here after the import, whose foreign expiry no longer describes it.
fn adoptable(stored: &[u8]) -> Result<Option<Device<'_>>> {
	let (owner, device, carried): TokenValue<'_> = deserialize_from_slice(stored)?;

	let adoptable = carried.is_none() && stored.len() == serialize_key((owner, device))?.len();

	Ok(adoptable.then_some((owner, device)))
}

/// Reads a stored token value, yielding its owner only when the row carries the
/// expiry the adoption stamped.
///
/// A token this server issued carries an expiry it computed itself, or none, so
/// a stored value equal to the origin's is the adoption's own write. A foreign
/// row, and a token a later login replaced, read as something else and are
/// left alone.
fn adopted(stored: &[u8], expires: u64) -> Result<Option<Device<'_>>> {
	let (owner, device, carried): TokenValue<'_> = deserialize_from_slice(stored)?;

	let adopted = carried == Some(expires);

	Ok(adopted.then_some((owner, device)))
}

#[cfg(test)]
mod tests {
	use ruma::{device_id, user_id};
	use tuwunel_database::{KeyBuf, deserialize_from_slice, serialize_key};

	use super::{Device, TokenValue, adoptable, adopted};

	const EXPIRES: u64 = 1_700_000_000;

	fn owner_device() -> Device<'static> {
		(user_id!("@alice:localhost"), device_id!("AAAAAAAAAA"))
	}

	/// The shape a foreign database writes: owner and device, no expiry field.
	fn foreign() -> KeyBuf {
		serialize_key(owner_device()).expect("the foreign value serializes")
	}

	/// The shape this server writes, whose expiry field is present either way.
	fn native(expires: Option<u64>) -> KeyBuf {
		let (owner, device) = owner_device();

		serialize_key((owner, device, expires)).expect("the native value serializes")
	}

	#[test]
	fn foreign_value_reads_as_non_expiring() {
		let (owner, device) = owner_device();
		let foreign = foreign();

		let (read_owner, read_device, carried): TokenValue<'_> =
			deserialize_from_slice(&foreign).expect("the foreign value deserializes");

		assert_eq!(read_owner, owner);
		assert_eq!(read_device, device);
		assert_eq!(carried, None, "a row without the tail must read as non-expiring");
	}

	#[test]
	fn adopted_value_carries_its_expiry() {
		let (owner, device) = owner_device();
		let stamped = native(Some(EXPIRES));

		let (read_owner, read_device, carried): TokenValue<'_> =
			deserialize_from_slice(&stamped).expect("the stamped value deserializes");

		assert_eq!(read_owner, owner);
		assert_eq!(read_device, device);
		assert_eq!(carried, Some(EXPIRES));
	}

	#[test]
	fn a_past_expiry_survives_the_round_trip() {
		let stamped = native(Some(1));

		let (.., carried): TokenValue<'_> =
			deserialize_from_slice(&stamped).expect("the stamped value deserializes");

		assert_eq!(carried, Some(1), "an expiry already past is carried, not clamped");
	}

	#[test]
	fn a_foreign_row_is_adoptable() {
		let (owner, device) = owner_device();
		let foreign = foreign();

		let (read_owner, read_device) = adoptable(&foreign)
			.expect("the foreign value is readable")
			.expect("the foreign value is adoptable");

		assert_eq!(read_owner, owner);
		assert_eq!(read_device, device);
	}

	// A token issued after the import must never be given a stale expiry.
	#[test]
	fn a_row_this_server_wrote_is_refused() {
		let native = native(None);
		let candidate = adoptable(&native).expect("the native value is readable");

		assert!(candidate.is_none());
	}

	#[test]
	fn a_row_already_carrying_an_expiry_is_refused() {
		let expiring = native(Some(EXPIRES));
		let candidate = adoptable(&expiring).expect("the expiring value is readable");

		assert!(candidate.is_none());
	}

	#[test]
	fn an_adopted_row_is_restorable() {
		let (owner, device) = owner_device();
		let stamped = native(Some(EXPIRES));

		let (read_owner, read_device) = adopted(&stamped, EXPIRES)
			.expect("the stamped value is readable")
			.expect("the stamped value is restorable");

		assert_eq!(read_owner, owner);
		assert_eq!(read_device, device);
	}

	#[test]
	fn a_row_carrying_another_expiry_is_kept() {
		let expiring = native(Some(EXPIRES.saturating_add(1)));
		let candidate = adopted(&expiring, EXPIRES).expect("the expiring value is readable");

		assert!(candidate.is_none());
	}

	#[test]
	fn a_foreign_row_is_not_restorable() {
		let foreign = foreign();
		let candidate = adopted(&foreign, EXPIRES).expect("the foreign value is readable");

		assert!(candidate.is_none());
	}

	#[test]
	fn a_row_without_an_expiry_is_kept() {
		let native = native(None);
		let candidate = adopted(&native, EXPIRES).expect("the native value is readable");

		assert!(candidate.is_none());
	}
}
