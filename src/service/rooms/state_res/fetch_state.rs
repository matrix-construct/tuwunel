use ruma::{
	UserId,
	events::{StateEventType, room::member::MembershipState},
};
use tuwunel_core::{
	Result, err,
	matrix::{Event, StateKey},
};

use super::events::{
	JoinRule, RoomCreateEvent, RoomJoinRulesEvent, RoomMemberEvent, RoomPowerLevelsEvent,
	RoomThirdPartyInviteEvent, member::RoomMemberEventResultExt,
};

pub(super) trait FetchStateExt<Pdu: Event> {
	async fn room_create_event(&self) -> Result<RoomCreateEvent<Pdu>>;

	async fn user_membership(&self, user_id: &UserId) -> Result<MembershipState>;

	async fn room_power_levels_event(&self) -> Option<RoomPowerLevelsEvent<Pdu>>;

	async fn join_rule(&self) -> Result<JoinRule>;

	async fn room_third_party_invite_event(
		&self,
		token: &str,
	) -> Option<RoomThirdPartyInviteEvent<Pdu>>;
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
			.map(RoomCreateEvent::new)
			.map_err(|e| err!("no `m.room.create` event in current state: {e}"))
	}

	async fn user_membership(&self, user_id: &UserId) -> Result<MembershipState> {
		self(StateEventType::RoomMember, user_id.as_str().into())
			.await
			.map(RoomMemberEvent::new)
			.membership()
	}

	async fn room_power_levels_event(&self) -> Option<RoomPowerLevelsEvent<Pdu>> {
		self(StateEventType::RoomPowerLevels, "".into())
			.await
			.map(RoomPowerLevelsEvent::new)
			.ok()
	}

	async fn join_rule(&self) -> Result<JoinRule> {
		self(StateEventType::RoomJoinRules, "".into())
			.await
			.map(RoomJoinRulesEvent::new)
			.map_err(|e| err!("no `m.room.join_rules` event in current state: {e}"))?
			.join_rule()
	}

	async fn room_third_party_invite_event(
		&self,
		token: &str,
	) -> Option<RoomThirdPartyInviteEvent<Pdu>> {
		self(StateEventType::RoomThirdPartyInvite, token.into())
			.await
			.ok()
			.map(RoomThirdPartyInviteEvent::new)
	}
}
