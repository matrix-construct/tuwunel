//! Helper traits and types to work with events (aka PDUs).

pub mod create;
pub mod join_rules;
pub mod member;
pub mod power_levels;
pub mod third_party_invite;

pub use self::{
	create::RoomCreateEvent,
	join_rules::{JoinRule, RoomJoinRulesEvent},
	member::{RoomMemberEvent, RoomMemberEventContent},
	power_levels::{RoomPowerLevelsEvent, RoomPowerLevelsIntField},
	third_party_invite::RoomThirdPartyInviteEvent,
};

/// Whether the given event is a power event.
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
pub(super) fn is_power_event<Pdu>(event: &Pdu) -> bool
where
	Pdu: tuwunel_core::matrix::Event,
{
	use ruma::events::{TimelineEventType, room::member::MembershipState};

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
