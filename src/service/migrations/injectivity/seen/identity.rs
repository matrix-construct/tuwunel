use std::collections::BTreeSet;

use futures::{TryStreamExt, future::try_join};
use tuwunel_core::{
	Result,
	utils::{IterStream, TryReadyExt, stream::TryWidebandExt},
};
use tuwunel_database::{Database, KeyBuf, Map, Slice, Txn, serialize_key, serialize_val};

use crate::{
	Services,
	integrity::{
		ScanExt,
		short_ids::{
			identity::{Candidate, Family, Identities, Kind, admitted, census},
			references::References,
		},
	},
};

const CACHE_BATCH: usize = 64;

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn repair(services: &Services) -> Result<Identities> {
	clear_cache(services, "authchainkey_authchain").await?;
	let identities = census(services).await?;
	let identities = match heal(services, &identities).await? {
		| false => identities,
		| true => census(services).await?,
	};

	let references = References::census(services, &identities).await?;

	let events =
		cleanup(services, &identities.events, &references.events, references.event_complete)
			.await?;

	let statekeys = cleanup(
		services,
		&identities.statekeys,
		&references.statekeys,
		references.statekey_complete,
	)
	.await?;

	match events || statekeys {
		| false => Ok(identities),
		| true => census(services).await,
	}
}

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn clear_cache(services: &Services, column: &str) -> Result {
	let db = &services.db;
	let map = &db[column];

	map.raw_keys()
		.scanned(&services.server)
		.map_ok(KeyBuf::from_slice)
		.try_chunks(CACHE_BATCH)
		.map_err(|error| error.1)
		.ready_try_for_each(|keys| deletion(db, map, keys).try_write())
		.await?;

	// One barrier covers every batch; a cache left partly cleared is cleared again on retry.
	db.engine.sync()
}

fn deletion(db: &Database, map: &Map, keys: impl IntoIterator<Item = impl AsRef<Slice>>) -> Txn {
	keys.into_iter().fold(db.txn(), |mut txn, key| {
		txn.del_raw(map, key);
		txn
	})
}

#[tracing::instrument(level = "debug", skip_all)]
async fn heal(services: &Services, identities: &Identities) -> Result<bool> {
	let db = &services.db;
	let Identities { events, statekeys } = identities;
	let (event_heals, statekey_heals) =
		try_join(admissible(db, events, healable), admissible(db, statekeys, healable)).await?;

	if event_heals.len() == 0 && statekey_heals.len() == 0 {
		return Ok(false);
	}

	// A persisted summary omits entries a heal makes resolvable, so it clears before any write.
	clear_cache(services, "roomid_spacehierarchy").await?;

	event_heals
		.map(|candidate| (events, candidate))
		.chain(statekey_heals.map(|candidate| (statekeys, candidate)))
		.try_for_each(|(family, candidate)| {
			services.server.check_running()?;
			write(db, family, candidate)
		})
		.map(|()| true)
}

fn healable(candidate: &Candidate) -> bool {
	matches!(candidate.kind, Kind::Reverse | Kind::Forward)
}

fn write(db: &Database, family: &Family, candidate: &Candidate) -> Result {
	let short = serialize_key(candidate.short)?;
	let identity = serialize_val(candidate.identity.as_slice())?;
	let txn = match candidate.kind {
		| Kind::Reverse => Txn::insert(&db[family.reverse], [(short, identity)]),
		| _ => Txn::insert(&db[family.forward], [(identity, short)]),
	};

	txn.try_execute().map_err(Into::into)
}

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn cleanup(
	services: &Services,
	family: &Family,
	references: &BTreeSet<u64>,
	complete: bool,
) -> Result<bool> {
	if !complete {
		return Ok(false);
	}

	let db = &services.db;
	let eligible = |candidate: &Candidate| {
		matches!(candidate.kind, Kind::Alias(_)) && !references.contains(&candidate.short)
	};

	let keys = admissible(db, family, eligible)
		.await?
		.map(|candidate| candidate.short.to_be_bytes());

	let changed = keys.len() != 0;

	services.server.check_running()?;
	deletion(db, &db[family.reverse], keys)
		.try_execute()
		.map(|()| changed)
		.map_err(Into::into)
}

async fn admissible<'a, F>(
	db: &Database,
	family: &'a Family,
	eligible: F,
) -> Result<impl ExactSizeIterator<Item = &'a Candidate>>
where
	F: Fn(&Candidate) -> bool + Send + Sync,
{
	// Collected whole, so every buffered admission error precedes the caller's writes.
	let candidates: Vec<_> = family
		.candidates
		.iter()
		.filter(|candidate| eligible(candidate))
		.try_stream()
		.wide_and_then(async |candidate| {
			let admit = admitted(db, family, candidate).await?;

			Ok(admit.then_some(candidate))
		})
		.ready_try_filter_map(Ok)
		.try_collect()
		.await?;

	Ok(candidates.into_iter())
}
