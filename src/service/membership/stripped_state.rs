use std::{
	borrow::Cow,
	collections::{HashMap, hash_map::Entry},
};

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedRoomId, RoomId, RoomVersionId, UserId,
	api::federation::membership::RawStrippedState,
	events::{AnyStrippedStateEvent, StateEventType, StateKey},
	room_version_rules::RoomIdFormatVersion,
	serde::{JsonObject, Raw},
};
use serde::Deserialize;
use tuwunel_core::{Event, PduEvent, Result, implement, matrix::event::gen_event_id};

use super::Service;

/// The `(type, state_key)` pair naming one cell of a room's state.
type StateCell = (StateEventType, StateKey);

/// The chosen entry per cell, and the entries chosen so far in array order.
type Accumulator = (HashMap<StateCell, usize>, Vec<RawStrippedState>);

/// The cell an entry names, borrowed where its JSON needs no unescaping.
#[derive(Deserialize)]
struct Cell<'a> {
	#[serde(rename = "type", borrow)]
	kind: Cow<'a, str>,

	#[serde(borrow)]
	state_key: Cow<'a, str>,
}

/// MSC4311 verdict for the create event carried in federated stripped state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrippedCreateVerdict {
	/// A full create PDU bound to the room with valid signatures.
	Valid,

	/// No `m.room.create` event was present.
	Missing,

	/// A create event was present only in the legacy stripped form.
	NotPdu,

	/// A create PDU was present but does not bind to the room.
	WrongRoom,

	/// A create PDU was present but failed signature or hash checks.
	BadSignature,
}

/// Whether a non-`Valid` verdict warrants rejecting an invite or dropping the
/// event from knock state, given the room version and operator policy.
#[must_use]
pub fn enforce_stripped_create(
	verdict: StrippedCreateVerdict,
	v12_room_ids: bool,
	enforce: bool,
) -> bool {
	use StrippedCreateVerdict::*;

	match verdict {
		| Valid => false,
		// A complete create PDU bound to a different room must fail for v12+
		// rooms even during the migration window (MSC4311 Migration).
		| WrongRoom => v12_room_ids || enforce,
		| Missing | NotPdu | BadSignature => enforce,
	}
}

/// Whether the room version derives room ids from the create event hash
/// (MSC4291, room version 12 and above), which changes how a create event
/// binds to its room.
#[must_use]
pub fn v12_room_ids(room_version: &RoomVersionId) -> bool {
	room_version
		.rules()
		.is_some_and(|rules| matches!(rules.room_id_format, RoomIdFormatVersion::V2))
}

/// Collapse a federated stripped-state array to one entry per state cell.
///
/// The array is state, so two entries sharing a cell are already malformed, and
/// which one a consumer obeys is otherwise decided by its own pick strategy
/// against an ordering the spec never gives.
///
/// A cell's first full PDU wins, and only a cell holding no PDU keeps its first
/// legacy entry. An entry naming no readable cell, missing either half, drops:
/// it addresses no state, and reading a missing `state_key` as the empty one
/// would let a non-state event occupy a real cell and displace it.
#[must_use]
pub fn dedup_stripped_state(state: Vec<RawStrippedState>) -> Vec<RawStrippedState> {
	let (_, kept) = state
		.into_iter()
		.filter_map(|entry| state_cell(&entry).map(|cell| (cell, entry)))
		.fold(Accumulator::default(), |(mut chosen, mut kept), (cell, entry)| {
			match chosen.entry(cell) {
				| Entry::Vacant(vacant) => {
					vacant.insert(kept.len());
					kept.push(entry);
				},
				| Entry::Occupied(occupied) => {
					let held = &mut kept[*occupied.get()];

					if is_legacy(held) && !is_legacy(&entry) {
						*held = entry;
					}
				},
			}

			(chosen, kept)
		});

	kept
}

/// Drop the entries occupying a user's membership cell.
///
/// The invite route appends its own copy of that event, whose sender the origin
/// check bound to the sending server, so a copy the sending server chose is
/// never the one to serve.
pub fn without_member(
	state: Vec<RawStrippedState>,
	user_id: &UserId,
) -> impl Iterator<Item = RawStrippedState> {
	state
		.into_iter()
		.filter(move |entry| !occupies_member_cell(entry, user_id))
}

fn state_cell(state: &RawStrippedState) -> Option<StateCell> {
	cell(state).map(|cell| (cell.kind.as_ref().into(), cell.state_key.as_ref().into()))
}

fn occupies_member_cell(state: &RawStrippedState, user_id: &UserId) -> bool {
	cell(state).is_some_and(|cell| {
		StateEventType::from(cell.kind.as_ref()) == StateEventType::RoomMember
			&& cell.state_key == user_id.as_str()
	})
}

/// The cell an entry names, borrowed out of the entry's own JSON.
///
/// Borrowing keeps a sender-supplied array off the heap for the comparisons
/// that only need to read a cell. [`state_cell`] pays for an owned copy, which
/// only the dedup map needs.
fn cell(state: &RawStrippedState) -> Option<Cell<'_>> {
	serde_json::from_str(entry_json(state)).ok()
}

#[expect(
	deprecated,
	reason = "Matrix 1.16 still permits receiving the legacy stripped variant for backwards \
	          compatibility."
)]
fn entry_json(state: &RawStrippedState) -> &str {
	match state {
		| RawStrippedState::Stripped(raw) => raw.json().get(),
		| RawStrippedState::Pdu(raw) => raw.get(),
	}
}

#[expect(
	deprecated,
	reason = "Matrix 1.16 still permits receiving the legacy stripped variant for backwards \
	          compatibility."
)]
fn is_legacy(state: &RawStrippedState) -> bool { matches!(state, RawStrippedState::Stripped(_)) }

/// Down-convert a federation stripped-state entry to the 4-field client shape,
/// reducing a full PDU to content, sender, optional state_key, and type.
#[expect(
	deprecated,
	reason = "Matrix 1.16 still permits receiving the legacy stripped variant for backwards \
	          compatibility."
)]
#[must_use]
pub fn into_client_stripped(
	room_id: &RoomId,
	state: RawStrippedState,
) -> Option<Raw<AnyStrippedStateEvent>> {
	match state {
		| RawStrippedState::Stripped(raw) => Some(raw),
		| RawStrippedState::Pdu(raw) => {
			let mut event: JsonObject = serde_json::from_str(raw.get()).ok()?;

			// PduEvent requires event_id and room_id; a v12 create PDU federates
			// with neither, and to_format() drops both from the stripped shape.
			event.insert("event_id".into(), "$placeholder".into());
			event
				.entry("room_id")
				.or_insert_with(|| room_id.as_str().into());

			let pdu: PduEvent = serde_json::from_value(event.into()).ok()?;

			Some(pdu.to_format())
		},
	}
}

/// Validate the `m.room.create` event in a federated invite's or knock's
/// stripped state against the stated room (MSC4311). Decision-free: callers map
/// the verdict to their own reject-or-warn policy.
#[implement(Service)]
#[expect(
	deprecated,
	reason = "Matrix 1.16 still permits receiving the legacy stripped variant for backwards \
	          compatibility."
)]
#[tracing::instrument(level = "debug", skip_all, fields(%room_id))]
pub async fn validate_stripped_create(
	&self,
	state: &[RawStrippedState],
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
) -> Result<StrippedCreateVerdict> {
	let create = state.iter().find_map(|event| match event {
		| RawStrippedState::Pdu(raw) => serde_json::from_str::<CanonicalJsonObject>(raw.get())
			.ok()
			.filter(is_create),
		| RawStrippedState::Stripped(_) => None,
	});

	let Some(mut create) = create else {
		let stripped = state.iter().any(|event| match event {
			| RawStrippedState::Stripped(raw) =>
				serde_json::from_str::<CanonicalJsonObject>(raw.json().get())
					.is_ok_and(|json| is_create(&json)),
			| RawStrippedState::Pdu(_) => false,
		});

		return Ok(match stripped {
			| true => StrippedCreateVerdict::NotPdu,
			| false => StrippedCreateVerdict::Missing,
		});
	};

	create.remove("unsigned");

	// Room-id binding: v12+ rooms hash the create event (MSC4291); earlier
	// versions compare the create event's room_id field.
	let bound = if v12_room_ids(room_version_id) {
		gen_event_id(&create, room_version_id)
			.ok()
			.and_then(|event_id| OwnedRoomId::from_parts('!', event_id.localpart(), None).ok())
			.is_some_and(|expected| expected == room_id)
	} else {
		create
			.get("room_id")
			.and_then(CanonicalJsonValue::as_str)
			.is_some_and(|id| id == room_id.as_str())
	};

	if !bound {
		return Ok(StrippedCreateVerdict::WrongRoom);
	}

	if self
		.services
		.server_keys
		.verify_event(&create, Some(room_version_id))
		.await
		.is_err()
	{
		return Ok(StrippedCreateVerdict::BadSignature);
	}

	Ok(StrippedCreateVerdict::Valid)
}

fn is_create(json: &CanonicalJsonObject) -> bool {
	let field = |key| json.get(key).and_then(CanonicalJsonValue::as_str);

	field("type") == Some("m.room.create") && field("state_key") == Some("")
}

#[cfg(test)]
#[expect(
	deprecated,
	reason = "Matrix 1.16 still permits receiving the legacy stripped variant for backwards \
	          compatibility."
)]
mod tests {
	use ruma::{
		api::federation::membership::RawStrippedState, events::StateEventType, serde::Raw,
		user_id,
	};
	use serde_json::{Value as JsonValue, json, value::RawValue as RawJsonValue};

	use super::{
		dedup_stripped_state, entry_json, is_legacy, occupies_member_cell, state_cell,
		without_member,
	};

	#[test]
	fn a_cells_first_pdu_wins_over_an_earlier_legacy_entry() {
		let deduped = dedup_stripped_state(vec![
			legacy(&create("@forged:example.org")),
			pdu(&create("@genuine:example.org")),
		]);

		assert_eq!(senders(&deduped), ["@genuine:example.org"]);
	}

	#[test]
	fn a_cell_holding_no_pdu_keeps_its_first_legacy_entry() {
		let deduped = dedup_stripped_state(vec![
			legacy(&create("@first:example.org")),
			legacy(&create("@second:example.org")),
		]);

		assert_eq!(senders(&deduped), ["@first:example.org"]);
	}

	#[test]
	fn distinct_cells_all_survive_in_order() {
		let deduped = dedup_stripped_state(vec![
			pdu(&create("@creator:example.org")),
			pdu(&member("@alice:example.org", "@alice:example.org")),
			pdu(&member("@bob:example.org", "@alice:example.org")),
		]);

		assert_eq!(deduped.len(), 3);
		assert_eq!(state_cell(&deduped[0]).expect("a cell").0, StateEventType::RoomCreate);
	}

	#[test]
	fn an_entry_without_a_readable_cell_drops() {
		let deduped = dedup_stripped_state(vec![
			pdu(&json!({"sender": "@alice:example.org", "content": {}})),
			pdu(&create("@creator:example.org")),
		]);

		assert_eq!(senders(&deduped), ["@creator:example.org"]);
	}

	#[test]
	fn the_invitees_membership_cell_never_survives() {
		let state = vec![
			pdu(&member("@invitee:example.org", "@forged:example.org")),
			pdu(&member("@other:example.org", "@alice:example.org")),
			pdu(&create("@creator:example.org")),
		];

		let kept: Vec<_> = without_member(state, user_id!("@invitee:example.org")).collect();

		assert_eq!(senders(&kept), ["@alice:example.org", "@creator:example.org"]);
	}

	#[test]
	fn an_all_legacy_array_survives_with_every_cell_intact() {
		let name = json!({
			"type": "m.room.name",
			"state_key": "",
			"sender": "@creator:example.org",
			"content": {"name": "a room"},
		});

		let deduped = dedup_stripped_state(vec![
			legacy(&create("@creator:example.org")),
			legacy(&member("@alice:example.org", "@alice:example.org")),
			legacy(&name),
		]);

		assert_eq!(deduped.len(), 3);
		assert!(deduped.iter().all(is_legacy));
		assert_eq!(senders(&deduped), [
			"@creator:example.org",
			"@alice:example.org",
			"@creator:example.org"
		]);
	}

	#[test]
	fn a_cell_spelled_with_escapes_still_reads() {
		// "m.room.member" with an escaped 'm', which a borrowing deserializer
		// cannot read out of the source buffer.
		let escaped = raw_pdu(
			r#"{"type":"m.room.\u006dember","state_key":"@invitee:example.org",
			   "sender":"@forged:example.org","content":{"membership":"invite"}}"#,
		);

		assert!(occupies_member_cell(&escaped, user_id!("@invitee:example.org")));

		let mut kept = without_member(vec![escaped], user_id!("@invitee:example.org"));

		assert!(kept.next().is_none());
	}

	fn legacy(event: &JsonValue) -> RawStrippedState {
		RawStrippedState::Stripped(
			Raw::new(event)
				.expect("valid json")
				.cast_unchecked(),
		)
	}

	fn raw_pdu(json: &str) -> RawStrippedState {
		RawStrippedState::Pdu(RawJsonValue::from_string(json.to_owned()).expect("valid json"))
	}

	fn pdu(event: &JsonValue) -> RawStrippedState {
		RawStrippedState::Pdu(
			Raw::<JsonValue>::new(event)
				.expect("valid json")
				.into_json(),
		)
	}

	fn create(sender: &str) -> JsonValue {
		json!({"type": "m.room.create", "state_key": "", "sender": sender, "content": {}})
	}

	fn member(user_id: &str, sender: &str) -> JsonValue {
		json!({
			"type": "m.room.member",
			"state_key": user_id,
			"sender": sender,
			"content": {"membership": "invite"},
		})
	}

	fn senders(state: &[RawStrippedState]) -> Vec<String> {
		state
			.iter()
			.map(|entry| {
				let value: JsonValue =
					serde_json::from_str(entry_json(entry)).expect("valid json");

				value["sender"]
					.as_str()
					.expect("a sender")
					.to_owned()
			})
			.collect()
	}
}
