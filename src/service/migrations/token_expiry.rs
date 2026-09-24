use std::{pin::pin, sync::Arc};

use futures::{TryFutureExt, TryStreamExt};
use ruma::{DeviceId, UserId};
use tuwunel_core::{Result, debug_warn, err, info, result::NotFound, warn};
use tuwunel_database::{KeyVal, Map, deserialize_from_slice, serialize_key};

use crate::Services;

type Device<'a> = (&'a UserId, &'a DeviceId);

type TokenValue<'a> = (&'a UserId, &'a DeviceId, Option<u64>);

/// Counts of what one run of the pass did.
///
/// A skipped row and an unreadable one are counted apart because only the
/// second withholds the marker, and withholding it retries the whole pass on the
/// next boot.
#[derive(Default)]
struct Tally {
	adopted: usize,
	skipped: usize,
	unreadable: usize,
}

/// Whether a row's adoption is carried out or only tested for.
#[derive(Clone, Copy)]
enum Mode {
	Probe,
	Adopt,
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
	let Some(tokenexpires) = services.db.open_cf("userdeviceid_tokenexpires")? else {
		return Ok(true);
	};

	let device_tokens = &services.db["userdeviceid_token"];
	let token_owners = &services.db["token_userdeviceid"];

	if services.oauth.get_server().is_err() {
		let mut adoptable = pin!(tokenexpires.raw_stream().try_filter_map(|row| {
			adopt_one(device_tokens, token_owners, row, Mode::Probe)
				.map_ok(|adoptable| adoptable.then_some(()))
		}));

		let waiting = adoptable.try_next().await?.is_some();

		if waiting {
			info!("Migrated OAuth sessions keep their origin lifetime; no OIDC provider");
		}

		return Ok(!waiting);
	}

	let cork = services.db.cork_and_sync();

	// One row at a time: each is read before it is rewritten, so a concurrent
	// pass could let two rows naming one token both clear the provenance guard.
	// A cursor error ends the walk rather than being counted, because the status
	// is sticky and the iterator cannot advance past it.
	let tally = tokenexpires
		.raw_stream()
		.try_fold(Tally::default(), async |tally, row| {
			Ok(tally.record(adopt_one(device_tokens, token_owners, row, Mode::Adopt).await))
		})
		.await?;

	drop(cork);

	let Tally { adopted, skipped, unreadable } = tally;

	// A skipped row is usually a device this pass correctly left alone, so the
	// summary counts them without implying a loss; a real loss logs per row.
	if adopted > 0 || skipped > 0 {
		info!(%adopted, %skipped, "Adopted token expiry from a foreign database");
	}

	// Leaving the marker unstamped is what makes an engine failure recoverable:
	// the pass is idempotent, so the next boot retries it whole.
	unreadable
		.eq(&0)
		.then_some(true)
		.ok_or_else(|| err!(Database("{unreadable} token expiries could not be read")))
}

impl Tally {
	fn record(mut self, result: Result<bool>) -> Self {
		match result {
			| Ok(true) => self.adopted = self.adopted.saturating_add(1),
			| Ok(false) => self.skipped = self.skipped.saturating_add(1),
			| Err(e) => {
				warn!(error = %e, "a token expiry could not be read");
				self.unreadable = self.unreadable.saturating_add(1);
			},
		}

		self
	}
}

/// Carries one device's expiry onto the token it names, reporting whether the
/// row was adoptable.
///
/// A `false` return is a row with nothing to carry: one this pass cannot make
/// sense of, a device holding no token here, or a value this server has already
/// written. Only a failed lookup returns an error, because a row that will never
/// decode would otherwise refuse every later boot as well. In probe mode the
/// verdict is returned without the write.
async fn adopt_one(
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

	let Ok(candidate) = adoptable(&stored) else {
		warn!(%user_id, %device_id, "skipping a token value that could not be read");
		return Ok(false);
	};

	let Some((owner, device)) = candidate else {
		debug_warn!(%user_id, %device_id, "skipping a token expiry this server already owns");
		return Ok(false);
	};

	if matches!(mode, Mode::Adopt) {
		token_owners.raw_put(token, (owner, device, Some(expires)));
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

#[cfg(test)]
mod tests {
	use ruma::{device_id, user_id};
	use tuwunel_database::{KeyBuf, deserialize_from_slice, serialize_key};

	use super::{Device, TokenValue, adoptable};

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
		let adopted = native(Some(EXPIRES));

		let (read_owner, read_device, carried): TokenValue<'_> =
			deserialize_from_slice(&adopted).expect("the adopted value deserializes");

		assert_eq!(read_owner, owner);
		assert_eq!(read_device, device);
		assert_eq!(carried, Some(EXPIRES));
	}

	#[test]
	fn a_past_expiry_survives_the_round_trip() {
		let adopted = native(Some(1));

		let (.., carried): TokenValue<'_> =
			deserialize_from_slice(&adopted).expect("the adopted value deserializes");

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
}
