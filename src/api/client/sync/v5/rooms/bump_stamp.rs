use futures::{TryStreamExt, pin_mut};
use ruma::{
	RoomId, UInt, UserId,
	events::TimelineEventType::{
		self, Beacon, CallInvite, PollStart, RoomEncrypted, RoomMessage, Sticker,
	},
};
use tuwunel_core::{
	Result, is_equal_to,
	matrix::{
		Event,
		pdu::{PduCount, PduEvent},
	},
	utils::TryReadyExt,
};
use tuwunel_service::Services;

/// MUST be sorted by `TimelineEventType::event_type_str()` for `binary_search`.
static DEFAULT_BUMP_TYPES: [TimelineEventType; 6] = [
	CallInvite,    // m.call.invite
	PollStart,     // m.poll.start
	RoomEncrypted, // m.room.encrypted
	RoomMessage,   // m.room.message
	Sticker,       // m.sticker
	Beacon,        // org.matrix.msc3672.beacon
];

pub(super) async fn room_bump_stamp(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	roomsince: PduCount,
	next_batch: PduCount,
	last_timeline_count: PduCount,
) -> Result<Option<UInt>> {
	if last_timeline_count <= roomsince {
		return Ok(None);
	}

	let bumpable_pdus = services
		.timeline
		.pdus_rev(Some(sender_user), room_id, Some(next_batch.saturating_add(1)))
		.ready_try_take_while(|&(pdu_count, _)| Ok(pdu_count > roomsince))
		.ready_try_filter_map(|(pdu_count, pdu)| {
			Ok(is_bumpable_pdu(&pdu, sender_user)
				.then(|| pdu_count.into_signed().try_into().ok())
				.flatten())
		});

	pin_mut!(bumpable_pdus);
	bumpable_pdus.try_next().await
}

fn is_bumpable_pdu(pdu: &PduEvent, sender_user: &UserId) -> bool {
	if pdu.is_redacted() {
		return false;
	}

	if *pdu.event_type() == TimelineEventType::RoomMember {
		return pdu
			.state_key()
			.is_some_and(is_equal_to!(sender_user.as_str()));
	}

	DEFAULT_BUMP_TYPES
		.binary_search(pdu.event_type())
		.is_ok()
}

#[cfg_attr(debug_assertions, tuwunel_core::ctor(unsafe))]
fn _is_sorted() {
	debug_assert!(
		DEFAULT_BUMP_TYPES.is_sorted(),
		"DEFAULT_BUMP_TYPES must be sorted by the developer"
	);
}

#[cfg(test)]
mod tests {
	use ruma::{
		CanonicalJsonObject, event_id, events::TimelineEventType, room_id, serde::Raw, uint,
		user_id,
	};
	use serde_json::{json, value::to_raw_value};
	use tuwunel_core::matrix::{StateKey, pdu::PduEvent};

	use super::{DEFAULT_BUMP_TYPES, is_bumpable_pdu};

	fn pdu(kind: TimelineEventType, state_key: Option<StateKey>, redacted: bool) -> PduEvent {
		let unsigned = redacted.then(|| {
			to_raw_value(&json!({ "redacted_because": {} }))
				.expect("valid unsigned")
				.into()
		});

		PduEvent {
			kind,
			content: Raw::from_json(
				to_raw_value(&CanonicalJsonObject::new()).expect("valid content"),
			),
			event_id: event_id!("$event:example.com").to_owned(),
			room_id: room_id!("!room:example.com").to_owned(),
			sender: user_id!("@alice:example.com").to_owned(),
			state_key,
			redacts: None,
			prev_events: Default::default(),
			auth_events: Default::default(),
			origin_server_ts: uint!(1),
			depth: uint!(1),
			hashes: Default::default(),
			origin: None,
			unsigned,
		}
	}

	#[test]
	fn default_bump_types_are_sorted() {
		assert!(DEFAULT_BUMP_TYPES.is_sorted());
	}

	#[test]
	fn default_bump_types_bump() {
		let sender = user_id!("@alice:example.com");

		for kind in DEFAULT_BUMP_TYPES.iter().cloned() {
			assert!(is_bumpable_pdu(&pdu(kind, None, false), sender));
		}
	}

	#[test]
	fn non_bump_type_does_not_bump() {
		let sender = user_id!("@alice:example.com");
		let pdu = pdu(TimelineEventType::RoomName, Some("".into()), false);

		assert!(!is_bumpable_pdu(&pdu, sender));
	}

	#[test]
	fn own_membership_bumps() {
		let sender = user_id!("@alice:example.com");
		let pdu = pdu(TimelineEventType::RoomMember, Some(sender.as_str().into()), false);

		assert!(is_bumpable_pdu(&pdu, sender));
	}

	#[test]
	fn other_membership_does_not_bump() {
		let sender = user_id!("@alice:example.com");
		let pdu = pdu(TimelineEventType::RoomMember, Some("@bob:example.com".into()), false);

		assert!(!is_bumpable_pdu(&pdu, sender));
	}

	#[test]
	fn redacted_pdu_does_not_bump() {
		let sender = user_id!("@alice:example.com");
		let pdu = pdu(TimelineEventType::RoomMessage, None, true);

		assert!(!is_bumpable_pdu(&pdu, sender));
	}
}
