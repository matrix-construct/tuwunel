use std::collections::BTreeSet;

use futures::{FutureExt, TryFutureExt, TryStreamExt};
use serde_json::from_slice;
use tuwunel_core::{
	Err, PduEvent, Result, err,
	utils::{IterStream, TryReadyExt, result::NotFound, stream::TryBroadbandExt},
};
use tuwunel_database::{Database, Txn};

use super::{
	Aliases, Census, Identity, Mapping, Services, claim, identity, lookup, present, short_of,
	statekey,
};

type Alias = (u64, (u64, Identity));

#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn recover(services: &Services, census: &Census) -> Result<Aliases> {
	if !census.complete || census.orphans.is_empty() {
		return Ok(Aliases::new());
	}

	let claimed: BTreeSet<u64> = services.db["statekey_shortstatekey"]
		.raw_stream()
		.ready_and_then(|row| services.server.check_running().map(|()| row))
		.inspect_ok(|_| services.server.progress.advance())
		.ready_try_filter_map(|(_, value)| {
			Ok(claim(value).filter(|short| census.orphans.contains_key(short)))
		})
		.try_collect()
		.await?;

	census
		.orphans
		.iter()
		.filter(|(short, _)| !claimed.contains(*short))
		.try_stream()
		.try_filter_map(|(short, events)| recover_one(services, census, *short, events))
		.try_collect()
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn recover_one(
	services: &Services,
	census: &Census,
	short: u64,
	events: &BTreeSet<u64>,
) -> Result<Option<Alias>> {
	services.server.check_running()?;
	let db = &services.db;
	let Some(statekey) = unanimous(db, &census.projection.events, events).await? else {
		return Ok(None);
	};

	let Some(winner) = lookup(db, "statekey_shortstatekey", &statekey)
		.await?
		.as_deref()
		.and_then(short_of)
	else {
		return Ok(None);
	};

	if identity(db, &census.projection.statekeys, winner, "shortstatekey_statekey")
		.await?
		.is_none_or(|(canonical, checked)| canonical != winner || checked != statekey)
	{
		return Ok(None);
	}

	let key = short.to_be_bytes();

	if present(db, "shortstatekey_statekey", &key).await? {
		return Ok(None);
	}

	Txn::insert(&db["shortstatekey_statekey"], [(key, statekey.as_slice())])
		.try_execute()
		.map_err(|error| err!("orphan identity publication: {error}"))?;

	if lookup(db, "shortstatekey_statekey", &key)
		.await?
		.is_none_or(|published| published != statekey)
	{
		return Err!("orphan identity verification failed");
	}

	Ok(Some((short, (winner, statekey))))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn unanimous(
	db: &Database,
	mapping: &Mapping,
	events: &BTreeSet<u64>,
) -> Result<Option<Identity>> {
	let Some(first) = events.first().copied() else {
		return Ok(None);
	};

	let Some(statekey) = accepted_occurrence(db, mapping, first).await? else {
		return Ok(None);
	};

	// A dissent decides the outcome, so dropping reads still pending is sound.
	let agreed = events
		.iter()
		.copied()
		.skip(1)
		.try_stream()
		.broad_try_all(|event| {
			accepted_occurrence(db, mapping, event)
				.map_ok(|other| other.as_ref() == Some(&statekey))
		})
		.await?;

	Ok(agreed.then_some(statekey))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn accepted_occurrence(
	db: &Database,
	mapping: &Mapping,
	event: u64,
) -> Result<Option<Identity>> {
	let Some((_, event_id)) = identity(db, mapping, event, "shorteventid_eventid").await? else {
		return Ok(None);
	};

	accepted_key(db, &event_id).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn accepted_key(db: &Database, event_id: &[u8]) -> Result<Option<Identity>> {
	let Some(pdu_id) = lookup(db, "eventid_pduid", event_id).await? else {
		return Ok(None);
	};

	let Some(pdu) = db["pduid_pdu"]
		.get(&pdu_id)
		.map(NotFound::present)
		.await?
	else {
		return Ok(None);
	};

	// A full decode: only a well-formed accepted PDU may vote.
	let Ok(pdu) = from_slice::<PduEvent>(&pdu) else {
		return Ok(None);
	};

	let Some(state_key) = pdu.state_key.as_ref() else {
		return Ok(None);
	};

	if pdu.event_id.as_bytes() != event_id {
		return Ok(None);
	}

	Ok(Some(statekey(&pdu.kind, state_key)))
}
