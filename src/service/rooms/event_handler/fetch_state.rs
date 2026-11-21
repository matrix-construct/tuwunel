use std::collections::{HashMap, hash_map};

use futures::FutureExt;
use ruma::{EventId, OwnedEventId, RoomId, RoomVersionId, ServerName, events::StateEventType};
use serde::Deserialize;
use tuwunel_core::{Err, Result, debug, debug_warn, err, implement};
use tuwunel_matrix::{Event, PduEvent, pdu::check_room_id};

use crate::{
	fetcher::{Op, Opts},
	rooms::short::ShortStateKey,
};

#[cfg(test)]
mod tests;

#[derive(Deserialize)]
struct StateIdsResponse {
	pdu_ids: Vec<OwnedEventId>,
}

struct Assembly<'a> {
	origin: &'a ServerName,
	responder: &'a ServerName,
	room_id: &'a RoomId,
	room_version: &'a RoomVersionId,
	recursion_level: usize,
	create_event_id: &'a EventId,
}

/// Call /state_ids to find out what the state at this pdu is. We trust the
/// server's response to some extend (sic), but we still do a lot of checks
/// on the events
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(%origin),
)]
pub(super) async fn fetch_state(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	event_id: &EventId,
	room_version: &RoomVersionId,
	recursion_level: usize,
	create_event_id: &EventId,
) -> Result<Option<HashMap<u64, OwnedEventId>>> {
	let opts = Opts::new(Op::StateIds, room_id.to_owned())
		.event_id(event_id.to_owned())
		.hint(origin.to_owned())
		.attempt_limit(super::EVENT_FETCH_ATTEMPT_LIMIT)
		.fanout_for_op();

	let outcome = self
		.services
		.fetcher
		.fetch(opts)
		.await
		.inspect_err(|e| debug_warn!("Fetching state for event failed: {e}"))?;

	let StateIdsResponse { pdu_ids } = serde_json::from_slice(&outcome.bytes)
		.map_err(|e| err!(BadServerResponse("malformed state_ids response: {e}")))?;

	let assembly = Assembly {
		origin,
		responder: &outcome.origin,
		room_id,
		room_version,
		recursion_level,
		create_event_id,
	};

	self.assemble_state(pdu_ids, assembly)
		.await
		.map(Some)
}

/// Walks each distinct state ID and keys the events by type and state key.
///
/// A repeated ID names the same event again, so it is dropped before the walks;
/// two different events for one type and state key still refuse the state.
#[implement(super::Service)]
async fn assemble_state(
	&self,
	pdu_ids: Vec<OwnedEventId>,
	Assembly {
		origin,
		responder,
		room_id,
		room_version,
		recursion_level,
		create_event_id,
	}: Assembly<'_>,
) -> Result<HashMap<ShortStateKey, OwnedEventId>> {
	let (pdu_ids, repeated) = distinct(pdu_ids);

	if repeated > 0 {
		debug_warn!(%responder, %room_id, repeated, "Dropped repeated state IDs");
	}

	debug!("Fetching state events");
	let state_ids = pdu_ids.iter().map(AsRef::as_ref);
	let state_vec = self
		.fetch_auth(origin, room_id, state_ids, room_version, recursion_level)
		.boxed() // cold arm: federation fallback
		.await;

	let mut state: HashMap<ShortStateKey, OwnedEventId> = HashMap::with_capacity(state_vec.len());
	for (pdu, _) in state_vec {
		let state_key = room_state_key(&pdu, room_id)?;
		let event_type = pdu.event_type().to_cow_str().into();
		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&event_type, state_key)
			.await;

		match state.entry(shortstatekey) {
			| hash_map::Entry::Vacant(v) => {
				v.insert(pdu.event_id().to_owned());
			},
			| hash_map::Entry::Occupied(_) => {
				return Err!(Request(InvalidParam(
					"State event's type and state_key ({:?},{:?}) exists multiple times.",
					pdu.event_type(),
					state_key,
				)));
			},
		}
	}

	// The original create event must still be in the state
	let create_shortstatekey = self
		.services
		.short
		.get_shortstatekey(&StateEventType::RoomCreate, "")
		.await?;

	if state
		.get(&create_shortstatekey)
		.map(AsRef::as_ref)
		!= Some(create_event_id)
	{
		return Err!(Database("Incoming event refers to wrong create event."));
	}

	Ok(state)
}

fn distinct(mut pdu_ids: Vec<OwnedEventId>) -> (Vec<OwnedEventId>, usize) {
	let received = pdu_ids.len();

	pdu_ids.sort_unstable();
	pdu_ids.dedup();

	let repeated = received.saturating_sub(pdu_ids.len());

	(pdu_ids, repeated)
}

fn room_state_key<'a>(pdu: &'a PduEvent, room_id: &RoomId) -> Result<&'a str> {
	check_room_id(pdu, room_id)?;
	pdu.state_key()
		.ok_or_else(|| err!(Database("Found non-state pdu in state events.")))
}
