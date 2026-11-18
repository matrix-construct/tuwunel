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

#[derive(Deserialize)]
struct StateIdsResponse {
	pdu_ids: Vec<OwnedEventId>,
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

	debug!("Fetching state events");
	let state_ids = pdu_ids.iter().map(AsRef::as_ref);
	let state_vec = self
		.fetch_auth(origin, room_id, state_ids, room_version, recursion_level)
		.boxed() // cold arm: federation fallback
		.await;

	let mut state: HashMap<ShortStateKey, OwnedEventId> = HashMap::with_capacity(state_vec.len());
	for (pdu, _) in state_vec {
		let state_key = fetched_state_key(&pdu, room_id)?;

		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&pdu.kind().to_string().into(), state_key)
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

	Ok(Some(state))
}

fn fetched_state_key<'a>(pdu: &'a PduEvent, room_id: &RoomId) -> Result<&'a str> {
	check_room_id(pdu, room_id)?;
	pdu.state_key()
		.ok_or_else(|| err!(Database("Found non-state pdu in state events.")))
}

#[cfg(test)]
mod tests {
	use ruma::{event_id, room_id, user_id};
	use serde_json::{from_value, json};
	use tuwunel_core::Result;

	use super::fetched_state_key;
	use crate::test_utils::{pdu_json, with_state_key};

	#[test]
	fn a_fetched_state_key_is_scoped_to_its_room() -> Result {
		let room = room_id!("!one:example.com");
		let name = pdu_json(
			event_id!("$name"),
			room,
			user_id!("@alice:example.com"),
			"m.room.name",
			&json!({ "name": "Room one" }),
		);
		let pdu = from_value(with_state_key(name, ""))?;

		assert_eq!(fetched_state_key(&pdu, room)?, "");
		fetched_state_key(&pdu, room_id!("!two:example.com"))
			.expect_err("a state key cannot be taken from another room");
		Ok(())
	}
}
