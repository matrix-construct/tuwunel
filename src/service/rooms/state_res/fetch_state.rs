use ruma::{
	UserId,
	events::{StateEventType, room::member::MembershipState},
};
use tuwunel_core::{
	Result, err,
	matrix::{Event, StateKey},
	result::NotFound,
};

use super::{
	event_auth::auth_input_error,
	events::{
		JoinRule, RoomCreateEvent, RoomJoinRulesEvent, RoomMemberEvent, RoomPowerLevelsEvent,
		RoomThirdPartyInviteEvent, member::RoomMemberEventResultExt,
	},
};

pub(super) trait FetchStateExt<Pdu: Event> {
	async fn room_create_event(&self) -> Result<RoomCreateEvent<Pdu>>;

	async fn user_membership(&self, user_id: &UserId) -> Result<MembershipState>;

	async fn room_power_levels_event(&self) -> Result<Option<RoomPowerLevelsEvent<Pdu>>>;

	async fn join_rule(&self) -> Result<JoinRule>;

	async fn room_third_party_invite_event(
		&self,
		token: &str,
	) -> Result<Option<RoomThirdPartyInviteEvent<Pdu>>>;
}

impl<Fetch, Fut, Pdu> FetchStateExt<Pdu> for &Fetch
where
	Fetch: Fn(StateEventType, StateKey) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>>,
	Pdu: Event,
{
	async fn room_create_event(&self) -> Result<RoomCreateEvent<Pdu>> {
		self(StateEventType::RoomCreate, "".into())
			.await
			.map_err(auth_input_error)
			.map(RoomCreateEvent::new)
			.map_err(|error| {
				if error.is_not_found() {
					err!("no `m.room.create` event in current state: {error}")
				} else {
					error
				}
			})
	}

	async fn user_membership(&self, user_id: &UserId) -> Result<MembershipState> {
		self(StateEventType::RoomMember, user_id.as_str().into())
			.await
			.map_err(auth_input_error)
			.map(RoomMemberEvent::new)
			.membership()
			.map_err(auth_input_error)
	}

	async fn room_power_levels_event(&self) -> Result<Option<RoomPowerLevelsEvent<Pdu>>> {
		self(StateEventType::RoomPowerLevels, "".into())
			.await
			.map_err(auth_input_error)
			.map(RoomPowerLevelsEvent::new)
			.optional()
	}

	async fn join_rule(&self) -> Result<JoinRule> {
		let event = self(StateEventType::RoomJoinRules, "".into())
			.await
			.map_err(auth_input_error)
			.map(RoomJoinRulesEvent::new)
			.map_err(|error| {
				if error.is_not_found() {
					err!("no `m.room.join_rules` event in current state: {error}")
				} else {
					error
				}
			})?;

		event.join_rule().map_err(auth_input_error)
	}

	async fn room_third_party_invite_event(
		&self,
		token: &str,
	) -> Result<Option<RoomThirdPartyInviteEvent<Pdu>>> {
		self(StateEventType::RoomThirdPartyInvite, token.into())
			.await
			.map_err(auth_input_error)
			.map(RoomThirdPartyInviteEvent::new)
			.optional()
	}
}
