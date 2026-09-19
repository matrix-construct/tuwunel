//! Helper traits and types to work with events (aka PDUs).

pub mod create;
pub mod join_rules;
pub mod member;
pub mod power_levels;
pub mod third_party_invite;

use std::borrow::Cow;

use ruma::events::room::member::MembershipState;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::matrix::StateKey;

pub use self::{
	create::RoomCreateEvent,
	join_rules::{JoinRule, RoomJoinRulesEvent},
	member::{RoomMemberEvent, RoomMemberEventContent},
	power_levels::{RoomPowerLevelsEvent, RoomPowerLevelsIntField},
	third_party_invite::RoomThirdPartyInviteEvent,
};

/// Whether a stored event is a power event, decoded from the fields that decide
/// it.
///
/// Definition in the spec:
///
/// > A power event is a state event with type `m.room.power_levels` or
/// > `m.room.join_rules`, or a
/// > state event with type `m.room.member` where the `membership` is `leave` or
/// > `ban` and the
/// > `sender` does not match the `state_key`. The idea behind this is that
/// > power events are events
/// > that might remove someone’s ability to do something in the room.
///
/// `m.room.create` is included, extending the spec definition to match Synapse
/// and Ruma.
pub(super) struct PowerEvent(pub(super) bool);

#[derive(Deserialize)]
struct PowerEventFields<'a> {
	#[serde(rename = "type")]
	kind: PowerEventType,
	#[serde(borrow)]
	sender: Cow<'a, str>,
	state_key: Option<StateKey>,
	#[serde(borrow)]
	content: &'a RawJsonValue,
}

#[derive(Deserialize)]
enum PowerEventType {
	#[serde(rename = "m.room.create")]
	Create,
	#[serde(rename = "m.room.power_levels")]
	PowerLevels,
	#[serde(rename = "m.room.join_rules")]
	JoinRules,
	#[serde(rename = "m.room.member")]
	Member,
	#[serde(other)]
	Other,
}

impl<'de> Deserialize<'de> for PowerEvent {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let event = PowerEventFields::deserialize(deserializer)?;
		let state_key = event.state_key.as_deref();
		let is_power = match event.kind {
			| PowerEventType::Other => false,
			| PowerEventType::Create
			| PowerEventType::PowerLevels
			| PowerEventType::JoinRules => state_key == Some(""),
			| PowerEventType::Member =>
				is_power_membership(event.content) && Some(&*event.sender) != state_key,
		};

		Ok(Self(is_power))
	}
}

fn is_power_membership(content: &RawJsonValue) -> bool {
	RoomMemberEventContent::new(content)
		.membership()
		.is_ok_and(|membership| {
			matches!(membership, MembershipState::Leave | MembershipState::Ban)
		})
}

/// Whether the given event is a power event.
///
/// Operates on a fully decoded event; `PowerEvent` must classify identically.
#[cfg(test)]
pub(super) fn is_power_event<Pdu>(event: &Pdu) -> bool
where
	Pdu: tuwunel_core::matrix::Event,
{
	use ruma::events::TimelineEventType;

	match event.event_type() {
		| TimelineEventType::RoomPowerLevels
		| TimelineEventType::RoomJoinRules
		| TimelineEventType::RoomCreate => event.state_key() == Some(""),
		| TimelineEventType::RoomMember => {
			let content = RoomMemberEventContent::new(event.content());
			if content.membership().is_ok_and(|membership| {
				matches!(membership, MembershipState::Leave | MembershipState::Ban)
			}) {
				return Some(event.sender().as_str()) != event.state_key();
			}

			false
		},
		| _ => false,
	}
}
