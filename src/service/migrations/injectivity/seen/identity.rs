use std::{
	collections::{BTreeMap, BTreeSet},
	str::from_utf8,
	sync::Arc,
};

use futures::{FutureExt, StreamExt, TryStreamExt, future::lazy};
use ruma::EventId;
use tuwunel_core::{
	Result, err,
	utils::{IterStream, TryReadyExt, result::NotFound, stream::TryWidebandExt},
};
use tuwunel_database::{Database, KeyBuf, Map, SEP, Slice, Txn, serialize_key, serialize_val};

use super::{Identity, references::References, short_of};
use crate::{Services, migrations::scan::ScanExt};

pub(super) struct Identities {
	pub(super) events: Family,
	pub(super) statekeys: Family,
}

pub(super) struct Family {
	pub(super) candidates: Vec<Candidate>,
	pub(super) malformed: u64,
	forward: &'static str,
	reverse: &'static str,

	/// Number of forward rows claiming each candidate short or alias winner.
	claims: BTreeMap<u64, u64>,

	/// Number of reverse rows naming each identity whose forward row is absent.
	reverse_claims: BTreeMap<Identity, u64>,
}

pub(super) struct Candidate {
	pub(super) short: u64,
	pub(super) identity: Identity,
	pub(super) kind: Kind,
}

/// How a candidate row departs from a consistent identity pair.
///
/// Healing writes the missing row of an admitted `Reverse` or `Forward`
/// candidate, and cleanup deletes the reverse row of an admitted `Alias`.
/// Nothing writes an `Unresolved` row.
#[derive(Clone, Copy)]
pub(super) enum Kind {
	/// The forward row names a short that has no reverse row.
	Reverse,

	/// The reverse row names an identity that has no forward row.
	Forward,

	/// The reverse row names an identity whose forward row names a different
	/// short, carried here as the winner.
	Alias(u64),

	/// The row is malformed or contradicts its counterpart.
	Unresolved,
}

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

	cleanup(services, &identities.events, &references.events, references.event_complete).await?;

	cleanup(
		services,
		&identities.statekeys,
		&references.statekeys,
		references.statekey_complete,
	)
	.await?;

	census(services).await
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
pub(super) async fn census(services: &Services) -> Result<Identities> {
	let events = family(services, "eventid_shorteventid", "shorteventid_eventid").await?;
	let statekeys = family(services, "statekey_shortstatekey", "shortstatekey_statekey").await?;

	Ok(Identities { events, statekeys })
}

#[tracing::instrument(level = "debug", skip_all)]
async fn family(
	services: &Services,
	forward: &'static str,
	reverse: &'static str,
) -> Result<Family> {
	let db = &services.db;
	let dangling = db[forward]
		.raw_stream()
		.scanned(&services.server)
		.map_ok(|(identity, short)| decode(short, identity))
		.wide_and_then(async |(short, identity)| {
			let kind = match malformed(forward, short, &identity) {
				| true => Some(Kind::Unresolved),
				| false => reverse_kind(&db[reverse], short, &identity).await?,
			};

			Ok(kind.map(|kind| Candidate { short, identity, kind }))
		});

	// Construction opens a cursor, so the reverse one is deferred until the forward scan ends.
	let unpointed = lazy(|_| db[reverse].raw_stream())
		.flatten_stream()
		.scanned(&services.server)
		.map_ok(|(short, identity)| decode(short, identity))
		.wide_and_then(async |(short, identity)| {
			let kind = match malformed(forward, short, &identity) {
				| true => Some(Kind::Unresolved),
				| false => forward_kind(&db[forward], short, &identity).await?,
			};

			Ok(kind.map(|kind| Candidate { short, identity, kind }))
		});

	let candidates: Vec<_> = dangling
		.chain(unpointed)
		.ready_try_filter_map(Ok)
		.try_collect()
		.await?;

	let claims: BTreeMap<u64, u64> = candidates
		.iter()
		.flat_map(|candidate| {
			let winner = match candidate.kind {
				| Kind::Alias(winner) => Some(winner),
				| _ => None,
			};

			[Some(candidate.short), winner]
				.into_iter()
				.flatten()
				.map(|short| (short, 0_u64))
		})
		.collect();

	let claims = count_claims(services, forward, claims).await?;

	let reverse_claims: BTreeMap<Identity, u64> = candidates
		.iter()
		.filter(|candidate| matches!(candidate.kind, Kind::Forward))
		.map(|candidate| (candidate.identity.clone(), 0_u64))
		.collect();

	let reverse_claims = count_reverse_claims(services, reverse, reverse_claims).await?;

	let malformed_rows = candidates
		.iter()
		.filter(|candidate| malformed(forward, candidate.short, &candidate.identity))
		.count()
		.try_into()
		.map_err(|_| err!("candidate count exceeds integer width"))?;

	Ok(Family {
		candidates,
		malformed: malformed_rows,
		forward,
		reverse,
		claims,
		reverse_claims,
	})
}

fn decode(short: &[u8], identity: &[u8]) -> (u64, Identity) {
	(short_of(short).unwrap_or_default(), Identity::from_slice(identity))
}

fn malformed(forward: &str, short: u64, identity: &[u8]) -> bool {
	short == 0 || !valid(identity, forward == "eventid_shorteventid")
}

fn valid(identity: &[u8], event: bool) -> bool {
	if event {
		return from_utf8(identity).is_ok_and(|event_id| <&EventId>::try_from(event_id).is_ok());
	}

	let Some(separator) = identity.iter().position(|byte| *byte == SEP) else {
		return false;
	};

	let (event_type, key) = identity.split_at(separator);

	!event_type.is_empty() && from_utf8(event_type).is_ok() && from_utf8(&key[1..]).is_ok()
}

#[tracing::instrument(level = "trace", skip_all)]
async fn reverse_kind(map: &Arc<Map>, short: u64, identity: &[u8]) -> Result<Option<Kind>> {
	map.get(&short.to_be_bytes())
		.map(NotFound::present)
		.await
		.map(|value| match value.as_deref() {
			| Some(stored) if stored == identity => None,
			| Some(_) => Some(Kind::Unresolved),
			| None => Some(Kind::Reverse),
		})
}

#[tracing::instrument(level = "trace", skip_all)]
async fn forward_kind(map: &Arc<Map>, short: u64, identity: &[u8]) -> Result<Option<Kind>> {
	map.get(identity)
		.map(NotFound::present)
		.await
		.map(|value| match value.as_deref().map(short_of) {
			| None => Some(Kind::Forward),
			| Some(Some(winner)) if winner == short => None,
			| Some(Some(winner)) => Some(Kind::Alias(winner)),
			| Some(None) => Some(Kind::Unresolved),
		})
}

#[tracing::instrument(level = "debug", skip_all)]
async fn count_claims(
	services: &Services,
	forward: &str,
	claims: BTreeMap<u64, u64>,
) -> Result<BTreeMap<u64, u64>> {
	// Only claims on candidate shorts are counted, so no candidates means no scan.
	if claims.is_empty() {
		return Ok(claims);
	}

	services.db[forward]
		.raw_stream()
		.scanned(&services.server)
		.ready_try_fold(claims, |mut claims: BTreeMap<u64, u64>, (_, value)| {
			// Prefix width counts an overlong value as a claim, which can only block a repair.
			if let Some(count) = value
				.get(..8)
				.and_then(short_of)
				.and_then(|short| claims.get_mut(&short))
			{
				*count = count.saturating_add(1);
			}

			Ok(claims)
		})
		.await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn count_reverse_claims(
	services: &Services,
	reverse: &str,
	claims: BTreeMap<Identity, u64>,
) -> Result<BTreeMap<Identity, u64>> {
	if claims.is_empty() {
		return Ok(claims);
	}

	services.db[reverse]
		.raw_stream()
		.scanned(&services.server)
		.ready_try_fold(claims, |mut claims: BTreeMap<Identity, u64>, (_, identity)| {
			if let Some(count) = claims.get_mut(identity) {
				*count = count.saturating_add(1);
			}

			Ok(claims)
		})
		.await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn heal(services: &Services, identities: &Identities) -> Result<bool> {
	let db = &services.db;
	let Identities { events, statekeys } = identities;
	let event_heals = admissible(db, events, healable).await?;
	let statekey_heals = admissible(db, statekeys, healable).await?;

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
) -> Result {
	if !complete {
		return Ok(());
	}

	let db = &services.db;
	let eligible = |candidate: &Candidate| {
		matches!(candidate.kind, Kind::Alias(_)) && !references.contains(&candidate.short)
	};

	let keys = admissible(db, family, eligible)
		.await?
		.map(|candidate| candidate.short.to_be_bytes());

	services.server.check_running()?;
	deletion(db, &db[family.reverse], keys)
		.try_execute()
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

#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn admitted(
	db: &Database,
	family: &Family,
	candidate: &Candidate,
) -> Result<bool> {
	let Candidate { short, identity, kind } = candidate;

	if malformed(family.forward, *short, identity) {
		return Ok(false);
	}

	let claims = |short| {
		family
			.claims
			.get(&short)
			.copied()
			.unwrap_or_default()
	};

	let (forward, reverse) = (&db[family.forward], &db[family.reverse]);
	let accepted = match *kind {
		| Kind::Unresolved => false,
		| Kind::Reverse =>
			claims(*short) == 1
				&& points_to(forward, identity, *short).await?
				&& absent(reverse, &short.to_be_bytes()).await?,
		| Kind::Forward =>
			claims(*short) == 0
				&& family.reverse_claims.get(identity) == Some(&1)
				&& names(reverse, *short, identity).await?
				&& absent(forward, identity).await?,
		| Kind::Alias(winner) =>
			winner != 0
				&& winner != *short
				&& claims(*short) == 0
				&& claims(winner) == 1
				&& points_to(forward, identity, winner).await?
				&& names(reverse, *short, identity).await?
				&& names(reverse, winner, identity).await?,
	};

	Ok(accepted)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn points_to(map: &Arc<Map>, identity: &[u8], short: u64) -> Result<bool> {
	map.get(identity)
		.map(NotFound::present)
		.await
		.map(|value| value.as_deref().and_then(short_of) == Some(short))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn absent(map: &Arc<Map>, key: &[u8]) -> Result<bool> {
	map.get(key)
		.map(NotFound::present)
		.await
		.map(|value| value.is_none())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn names(map: &Arc<Map>, short: u64, identity: &[u8]) -> Result<bool> {
	map.get(&short.to_be_bytes())
		.map(NotFound::present)
		.await
		.map(|value| value.as_deref() == Some(identity))
}
