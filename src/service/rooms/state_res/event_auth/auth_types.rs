use ruma::{
	UserId,
	events::{StateEventType, TimelineEventType, room::member::MembershipState},
	room_version_rules::AuthorizationRules,
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{Err, Result, arrayvec::ArrayVec, matrix::pdu::MAX_AUTH_EVENTS};

use super::super::{TypeStateKey, events::member::RoomMemberEventContent};

pub type AuthTypes = ArrayVec<TypeStateKey, MAX_AUTH_EVENTS>;

/// Get the list of [relevant auth events] required to authorize the event of
/// the given type.
///
/// Returns a list of `(event_type, state_key)` tuples.
///
/// # Errors
///
/// Returns an `Err(_)` if a field could not be deserialized because `content`
/// does not respect the expected format for the `event_type`.
///
/// [relevant auth events]: https://spec.matrix.org/latest/server-server-api/#auth-events-selection
pub fn auth_types_for_event(
	event_type: &TimelineEventType,
	sender: &UserId,
	state_key: Option<&str>,
	content: &RawJsonValue,
	rules: &AuthorizationRules,
	always_create: bool,
) -> Result<AuthTypes> {
	let mut auth_types = AuthTypes::new();

	// The `auth_events` for the `m.room.create` event in a room is empty.
	// For other events, it should be the following subset of the room state:
	//
	// - The `m.room.create` event.
	// - The current `m.room.power_levels` event, if any.
	// - The sender’s current `m.room.member` event, if any.
	if *event_type != TimelineEventType::RoomCreate {
		// v1-v11, the `m.room.create` event.
		if !rules.room_create_event_id_as_room_id || always_create {
			auth_types.push((StateEventType::RoomCreate, "".into()));
		}

		auth_types.push((StateEventType::RoomPowerLevels, "".into()));
		auth_types.push((StateEventType::RoomMember, sender.as_str().into()));
	}

	// If type is `m.room.member`:
	if *event_type == TimelineEventType::RoomMember {
		auth_types_for_member_event(&mut auth_types, state_key, content, rules)?;
	}

	Ok(auth_types)
}

fn auth_types_for_member_event(
	auth_types: &mut AuthTypes,
	state_key: Option<&str>,
	content: &RawJsonValue,
	rules: &AuthorizationRules,
) -> Result {
	// The target’s current `m.room.member` event, if any.
	let Some(state_key) = state_key else {
		return Err!("missing `state_key` field for `m.room.member` event");
	};

	let key = (StateEventType::RoomMember, state_key.into());
	if !auth_types.contains(&key) {
		auth_types.push(key);
	}

	let content = RoomMemberEventContent::new(content);
	let membership = content.membership()?;

	// If `membership` is `join`, `invite` or `knock`, the current
	// `m.room.join_rules` event, if any.
	if matches!(
		membership,
		MembershipState::Join | MembershipState::Invite | MembershipState::Knock
	) {
		let key = (StateEventType::RoomJoinRules, "".into());
		if !auth_types.contains(&key) {
			auth_types.push(key);
		}
	}

	// If `membership` is `invite` and `content` contains a `third_party_invite`
	// property, the current `m.room.third_party_invite` event with `state_key`
	// matching `content.third_party_invite.signed.token`, if any.
	if membership == MembershipState::Invite {
		let third_party_invite = content.third_party_invite()?;
		if let Some(third_party_invite) = third_party_invite {
			let token = third_party_invite.token()?.into();
			let key = (StateEventType::RoomThirdPartyInvite, token);
			if !auth_types.contains(&key) {
				auth_types.push(key);
			}
		}
	}

	// If `content.join_authorised_via_users_server` is present, and the room
	// version supports restricted rooms, then the `m.room.member` event with
	// `state_key` matching `content.join_authorised_via_users_server`.
	//
	// Note: And the membership is join (https://github.com/matrix-org/matrix-spec/pull/2100)
	if membership == MembershipState::Join && rules.restricted_join_rule {
		let join_authorised_via_users_server = content.join_authorised_via_users_server()?;
		if let Some(user_id) = join_authorised_via_users_server {
			let key = (StateEventType::RoomMember, user_id.as_str().into());
			if !auth_types.contains(&key) {
				auth_types.push(key);
			}
		}
	}

	Ok(())
}
