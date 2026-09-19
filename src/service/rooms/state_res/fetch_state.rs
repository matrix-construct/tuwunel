use std::{collections::BTreeMap, future::ready};

use futures::{FutureExt, TryFutureExt};
use ruma::{
	UserId,
	events::{StateEventType, room::member::MembershipState},
};
use tuwunel_core::{
	Error, Result, err,
	matrix::{Event, PduEvent, StateKey, TypeStateKey},
	result::NotFound,
};

use super::{
	StateMap,
	event_auth::auth_input_error,
	events::{
		JoinRule, RoomCreateEvent, RoomJoinRulesEvent, RoomMemberEvent, RoomPowerLevelsEvent,
		RoomThirdPartyInviteEvent, member::RoomMemberEventResultExt,
	},
};

/// Reads authorization state through a copyable handle.
///
/// Implementations may return owned events or borrow events held by the state snapshot.
pub trait FetchState: Copy + Send + Sync {
	/// Event representation returned by this state snapshot.
	///
	/// Borrowed representations avoid cloning events during authorization.
	type Pdu: Event;

	/// Reads the event for an exact state tuple.
	///
	/// Missing tuples return a not-found error under the snapshot's completeness policy.
	fn get(
		self,
		ty: StateEventType,
		key: StateKey,
	) -> impl Future<Output = Result<Self::Pdu>> + Send;

	/// Reads the room creation event.
	///
	/// Missing creation state is an authorization input error.
	fn room_create_event(
		self,
	) -> impl Future<Output = Result<RoomCreateEvent<Self::Pdu>>> + Send {
		self.get(StateEventType::RoomCreate, "".into())
			.map_err(auth_input_error)
			.map_ok(RoomCreateEvent::new)
			.map_err(required("m.room.create"))
	}

	/// Reads a user membership from the state snapshot.
	///
	/// Missing membership follows the member-event defaulting rules.
	fn user_membership(
		self,
		user_id: &UserId,
	) -> impl Future<Output = Result<MembershipState>> + Send {
		self.get(StateEventType::RoomMember, user_id.as_str().into())
			.map_err(auth_input_error)
			.map_ok(RoomMemberEvent::new)
			.map(RoomMemberEventResultExt::membership)
			.map_err(auth_input_error)
	}

	/// Reads the optional room power levels event.
	///
	/// Missing power levels return no event.
	fn room_power_levels_event(
		self,
	) -> impl Future<Output = Result<Option<RoomPowerLevelsEvent<Self::Pdu>>>> + Send {
		self.get(StateEventType::RoomPowerLevels, "".into())
			.map_err(auth_input_error)
			.map_ok(RoomPowerLevelsEvent::new)
			.map(NotFound::optional)
	}

	/// Reads the current room join rule.
	///
	/// Missing join-rule state is an authorization input error.
	fn join_rule(self) -> impl Future<Output = Result<JoinRule>> + Send {
		self.get(StateEventType::RoomJoinRules, "".into())
			.map_err(auth_input_error)
			.map_ok(RoomJoinRulesEvent::new)
			.map_err(required("m.room.join_rules"))
			.and_then(|event| ready(event.join_rule().map_err(auth_input_error)))
	}

	/// Reads the invitation matching a third-party token.
	///
	/// Missing invitation state returns no event.
	fn room_third_party_invite_event(
		self,
		token: &str,
	) -> impl Future<Output = Result<Option<RoomThirdPartyInviteEvent<Self::Pdu>>>> + Send {
		self.get(StateEventType::RoomThirdPartyInvite, token.into())
			.map_err(auth_input_error)
			.map_ok(RoomThirdPartyInviteEvent::new)
			.map(NotFound::optional)
	}
}

/// Promotes a missing required state event to an authorization input error.
fn required(name: &'static str) -> impl Fn(Error) -> Error {
	move |error| {
		if error.is_not_found() {
			err!("no `{name}` event in current state: {error}")
		} else {
			error
		}
	}
}

impl<'a> FetchState for &'a StateMap<PduEvent> {
	type Pdu = &'a PduEvent;

	fn get(
		self,
		ty: StateEventType,
		key: StateKey,
	) -> impl Future<Output = Result<Self::Pdu>> + Send {
		ready(
			BTreeMap::get(self, &(ty, key))
				.ok_or_else(|| err!(Request(NotFound("Missing state event")))),
		)
	}
}

impl<'a> FetchState for &'a [(TypeStateKey, PduEvent)] {
	type Pdu = &'a PduEvent;

	fn get(
		self,
		ty: StateEventType,
		key: StateKey,
	) -> impl Future<Output = Result<Self::Pdu>> + Send {
		ready(
			self.iter()
				.find(|((event_type, state_key), _)| *event_type == ty && *state_key == key)
				.map(|(_, event)| event)
				.ok_or_else(|| err!(Request(NotFound("Missing auth_event {ty:?},{key:?}")))),
		)
	}
}
