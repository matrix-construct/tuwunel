use std::borrow::Borrow;

use futures::future::{join, join3};
use ruma::{
	AnyKeyName, SigningKeyId, UserId,
	events::{StateEventType, room::member::MembershipState},
	room_version_rules::AuthorizationRules,
	serde::{Base64, base64::Standard},
	signatures::verify_canonical_json_bytes,
};
use tuwunel_core::{
	Err, Result, err, is_equal_to,
	matrix::{Event, StateKey},
};

#[cfg(test)]
mod tests;

#[cfg(test)]
use super::test_utils;
use super::{
	FetchStateExt,
	events::{
		JoinRule, RoomCreateEvent, RoomMemberEvent, RoomPowerLevelsIntField,
		member::ThirdPartyInvite, power_levels::RoomPowerLevelsEventOptionExt,
	},
};

/// Check whether the given event passes the `m.room.roomber` authorization
/// rules.
///
/// This assumes that `ruma_signatures::verify_event()` was called previously,
/// as some authorization rules depend on the signatures being valid on the
/// event.
#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn check_room_member<Fetch, Fut, Pdu>(
	room_member_event: &RoomMemberEvent<Pdu>,
	rules: &AuthorizationRules,
	room_create_event: &RoomCreateEvent<Pdu>,
	fetch_state: &Fetch,
) -> Result
where
	Fetch: Fn(StateEventType, StateKey) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	// Since v1, if there is no state_key property, or no membership property in
	// content, reject.
	let Some(state_key) = room_member_event.state_key() else {
		return Err!("missing `state_key` field in `m.room.member` event");
	};

	let target_user = <&UserId>::try_from(state_key)
		.map_err(|e| err!("invalid `state_key` field in `m.room.member` event: {e}"))?;

	let target_membership = room_member_event.membership()?;

	// These checks are done `in ruma_signatures::verify_event()`:
	//
	// Since v8, if content has a join_authorised_via_users_server property:
	//
	// - Since v8, if the event is not validly signed by the homeserver of the user
	//   ID denoted by the key, reject.

	match target_membership {
		// Since v1, if membership is join:
		| MembershipState::Join =>
			check_room_member_join(
				room_member_event,
				target_user,
				rules,
				room_create_event,
				fetch_state,
			)
			.await,

		// Since v1, if membership is invite:
		| MembershipState::Invite =>
			check_room_member_invite(
				room_member_event,
				target_user,
				rules,
				room_create_event,
				fetch_state,
			)
			.await,

		// Since v1, if membership is leave:
		| MembershipState::Leave =>
			check_room_member_leave(
				room_member_event,
				target_user,
				rules,
				room_create_event,
				fetch_state,
			)
			.await,

		// Since v1, if membership is ban:
		| MembershipState::Ban =>
			check_room_member_ban(
				room_member_event,
				target_user,
				rules,
				room_create_event,
				fetch_state,
			)
			.await,

		// Since v7, if membership is knock:
		| MembershipState::Knock if rules.knocking =>
			check_room_member_knock(room_member_event, target_user, rules, fetch_state).await,

		// Since v1, otherwise, the membership is unknown. Reject.
		| _ => Err!("unknown membership"),
	}
}

/// Check whether the given event passes the `m.room.member` authorization rules
/// with a membership of `join`.
#[tracing::instrument(level = "trace", skip_all)]
async fn check_room_member_join<Fetch, Fut, Pdu>(
	room_member_event: &RoomMemberEvent<Pdu>,
	target_user: &UserId,
	rules: &AuthorizationRules,
	room_create_event: &RoomCreateEvent<Pdu>,
	fetch_state: &Fetch,
) -> Result
where
	Fetch: Fn(StateEventType, StateKey) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let mut creators = room_create_event.creators(rules)?;

	let mut prev_events = room_member_event.prev_events();

	let prev_event_is_room_create_event = prev_events
		.next()
		.is_some_and(|event_id| event_id.borrow() == room_create_event.event_id().borrow());

	let prev_event_is_only_room_create_event =
		prev_event_is_room_create_event && prev_events.next().is_none();

	// v1-v10, if the only previous event is an m.room.create and the state_key is
	// the creator, allow.
	// Since v11, if the only previous event is an m.room.create and the state_key
	// is the sender of the m.room.create, allow.
	if prev_event_is_only_room_create_event && creators.any(is_equal_to!(*target_user)) {
		return Ok(());
	}

	// Since v1, if the sender does not match state_key, reject.
	if room_member_event.sender() != target_user {
		return Err!("sender of join event must match target user");
	}

	let (current_membership, join_rule) =
		join(fetch_state.user_membership(target_user), fetch_state.join_rule()).await;

	// Since v1, if the sender is banned, reject.
	let current_membership = current_membership?;
	if current_membership == MembershipState::Ban {
		return Err!("banned user cannot join room");
	}

	// v1-v6, if the join_rule is invite then allow if membership state is invite or
	// join.
	// Since v7, if the join_rule is invite or knock then allow if membership state
	// is invite or join.
	let join_rule = join_rule?;
	if (join_rule == JoinRule::Invite || rules.knocking && join_rule == JoinRule::Knock)
		&& matches!(current_membership, MembershipState::Invite | MembershipState::Join)
	{
		return Ok(());
	}

	// v8-v9, if the join_rule is restricted:
	// Since v10, if the join_rule is restricted or knock_restricted:
	if rules.restricted_join_rule && matches!(join_rule, JoinRule::Restricted)
		|| rules.knock_restricted_join_rule && matches!(join_rule, JoinRule::KnockRestricted)
	{
		// Since v8, if membership state is join or invite, allow.
		if matches!(current_membership, MembershipState::Join | MembershipState::Invite) {
			return Ok(());
		}

		// Since v8, if the join_authorised_via_users_server key in content is not a
		// user with sufficient permission to invite other users, reject.
		//
		// Otherwise, allow.
		let Some(authorized_via_user) = room_member_event.join_authorised_via_users_server()?
		else {
			// The field is absent, we cannot authorize.
			return Err!(
				"cannot join restricted room without `join_authorised_via_users_server` field \
				 if not invited"
			);
		};

		// The member needs to be in the room to have any kind of permission.
		let authorized_via_user_membership = fetch_state
			.user_membership(&authorized_via_user)
			.await?;

		if authorized_via_user_membership != MembershipState::Join {
			return Err!("`join_authorised_via_users_server` is not joined");
		}

		let room_power_levels_event = fetch_state.room_power_levels_event().await;

		let authorized_via_user_power_level =
			room_power_levels_event.user_power_level(&authorized_via_user, creators, rules)?;

		let invite_power_level = room_power_levels_event
			.get_as_int_or_default(RoomPowerLevelsIntField::Invite, rules)?;

		if authorized_via_user_power_level < invite_power_level {
			return Err!("`join_authorised_via_users_server` does not have enough power");
		}

		return Ok(());
	}

	// Since v1, if the join_rule is public, allow. Otherwise, reject.
	if join_rule != JoinRule::Public {
		return Err!("cannot join a room that is not `public`");
	}

	Ok(())
}

/// Check whether the given event passes the `m.room.member` authorization rules
/// with a membership of `invite`.
#[tracing::instrument(level = "trace", skip_all)]
async fn check_room_member_invite<Fetch, Fut, Pdu>(
	room_member_event: &RoomMemberEvent<Pdu>,
	target_user: &UserId,
	rules: &AuthorizationRules,
	room_create_event: &RoomCreateEvent<Pdu>,
	fetch_state: &Fetch,
) -> Result
where
	Fetch: Fn(StateEventType, StateKey) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let third_party_invite = room_member_event.third_party_invite()?;

	// Since v1, if content has a third_party_invite property:
	if let Some(third_party_invite) = third_party_invite {
		return check_third_party_invite(
			room_member_event,
			&third_party_invite,
			target_user,
			fetch_state,
		)
		.await;
	}

	let sender_user = room_member_event.sender();
	let (sender_membership, current_target_user_membership, room_power_levels_event) = join3(
		fetch_state.user_membership(sender_user),
		fetch_state.user_membership(target_user),
		fetch_state.room_power_levels_event(),
	)
	.await;

	// Since v1, if the sender’s current membership state is not join, reject.
	let sender_membership = sender_membership?;
	if sender_membership != MembershipState::Join {
		return Err!("cannot invite user if sender is not joined");
	}

	// Since v1, if target user’s current membership state is join or ban, reject.
	let current_target_user_membership = current_target_user_membership?;
	if matches!(current_target_user_membership, MembershipState::Join | MembershipState::Ban) {
		return Err!("cannot invite user that is joined or banned");
	}

	let creators = room_create_event.creators(rules)?;
	let sender_power_level =
		room_power_levels_event.user_power_level(room_member_event.sender(), creators, rules)?;

	let invite_power_level =
		room_power_levels_event.get_as_int_or_default(RoomPowerLevelsIntField::Invite, rules)?;

	// Since v1, if the sender’s power level is greater than or equal to the invite
	// level, allow. Otherwise, reject.
	if sender_power_level < invite_power_level {
		return Err!("sender does not have enough power to invite");
	}

	Ok(())
}

/// Check whether the `third_party_invite` from the `m.room.member` event passes
/// the authorization rules.
#[tracing::instrument(level = "trace", skip_all)]
async fn check_third_party_invite<Fetch, Fut, Pdu>(
	room_member_event: &RoomMemberEvent<Pdu>,
	third_party_invite: &ThirdPartyInvite,
	target_user: &UserId,
	fetch_state: &Fetch,
) -> Result
where
	Fetch: Fn(StateEventType, StateKey) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let current_target_user_membership = fetch_state.user_membership(target_user).await?;

	// Since v1, if target user is banned, reject.
	if current_target_user_membership == MembershipState::Ban {
		return Err!("cannot invite user that is banned");
	}

	// Since v1, if content.third_party_invite does not have a signed property,
	// reject. Since v1, if signed does not have mxid and token properties, reject.
	let third_party_invite_token = third_party_invite.token()?;
	let third_party_invite_mxid = third_party_invite.mxid()?;

	// Since v1, if mxid does not match state_key, reject.
	if target_user != third_party_invite_mxid {
		return Err!("third-party invite mxid does not match target user");
	}

	// Since v1, if there is no m.room.third_party_invite event in the current room
	// state with state_key matching token, reject.
	let Some(room_third_party_invite_event) = fetch_state
		.room_third_party_invite_event(third_party_invite_token)
		.await
	else {
		return Err!("no `m.room.third_party_invite` in room state matches the token");
	};

	// Since v1, if sender does not match sender of the m.room.third_party_invite,
	// reject.
	if room_member_event.sender() != room_third_party_invite_event.sender() {
		return Err!(
			"sender of `m.room.third_party_invite` does not match sender of `m.room.member`"
		);
	}

	let signatures = third_party_invite.signatures()?;
	let public_keys = room_third_party_invite_event.public_keys()?;
	let signed_canonical_json = third_party_invite.signed_canonical_json()?;

	// Since v1, if any signature in signed matches any public key in the
	// m.room.third_party_invite event, allow.
	for entity_signatures_value in signatures.values() {
		let Some(entity_signatures) = entity_signatures_value.as_object() else {
			return Err!(Request(InvalidParam(
				"unexpected format of `signatures` field in `third_party_invite.signed` of \
				 `m.room.member` event: expected a map of string to object, got \
				 {entity_signatures_value:?}"
			)));
		};

		// We will ignore any error from now on, we just want to find a signature that
		// can be verified from a public key.

		for (key_id, signature_value) in entity_signatures {
			let Ok(parsed_key_id) = <&SigningKeyId<AnyKeyName>>::try_from(key_id.as_str()) else {
				continue;
			};

			let Some(signature_str) = signature_value.as_str() else {
				continue;
			};

			let Ok(signature) = Base64::<Standard>::parse(signature_str) else {
				continue;
			};

			let algorithm = parsed_key_id.algorithm();
			for encoded_public_key in &public_keys {
				let Ok(public_key) = encoded_public_key.decode() else {
					continue;
				};

				if verify_canonical_json_bytes(
					&algorithm,
					&public_key,
					signature.as_bytes(),
					signed_canonical_json.as_bytes(),
				)
				.is_ok()
				{
					return Ok(());
				}
			}
		}
	}

	// Otherwise, reject.
	Err!(
		"no signature on third-party invite matches a public key in `m.room.third_party_invite` \
		 event"
	)
}

/// Check whether the given event passes the `m.room.member` authorization rules
/// with a membership of `leave`.
#[tracing::instrument(level = "trace", skip_all)]
async fn check_room_member_leave<Fetch, Fut, Pdu>(
	room_member_event: &RoomMemberEvent<Pdu>,
	target_user: &UserId,
	rules: &AuthorizationRules,
	room_create_event: &RoomCreateEvent<Pdu>,
	fetch_state: &Fetch,
) -> Result
where
	Fetch: Fn(StateEventType, StateKey) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let (sender_membership, room_power_levels_event, current_target_user_membership) = join3(
		fetch_state.user_membership(room_member_event.sender()),
		fetch_state.room_power_levels_event(),
		fetch_state.user_membership(target_user),
	)
	.await;

	let sender_membership = sender_membership?;

	// v1-v6, if the sender matches state_key, allow if and only if that user’s
	// current membership state is invite or join.
	// Since v7, if the sender matches state_key, allow if and only if that user’s
	// current membership state is invite, join, or knock.
	if room_member_event.sender() == target_user {
		let membership_is_invite_or_join =
			matches!(sender_membership, MembershipState::Join | MembershipState::Invite);
		let membership_is_knock = rules.knocking && sender_membership == MembershipState::Knock;

		return if membership_is_invite_or_join || membership_is_knock {
			Ok(())
		} else {
			Err!("cannot leave if not joined, invited or knocked")
		};
	}

	// Since v1, if the sender’s current membership state is not join, reject.
	if sender_membership != MembershipState::Join {
		return Err!("cannot kick if sender is not joined");
	}

	let creators = room_create_event.creators(rules)?;
	let current_target_user_membership = current_target_user_membership?;

	let sender_power_level = room_power_levels_event.user_power_level(
		room_member_event.sender(),
		creators.clone(),
		rules,
	)?;

	let ban_power_level =
		room_power_levels_event.get_as_int_or_default(RoomPowerLevelsIntField::Ban, rules)?;

	// Since v1, if the target user’s current membership state is ban, and the
	// sender’s power level is less than the ban level, reject.
	if current_target_user_membership == MembershipState::Ban
		&& sender_power_level < ban_power_level
	{
		return Err!("sender does not have enough power to unban");
	}

	let kick_power_level =
		room_power_levels_event.get_as_int_or_default(RoomPowerLevelsIntField::Kick, rules)?;

	let target_user_power_level =
		room_power_levels_event.user_power_level(target_user, creators, rules)?;

	// Since v1, if the sender’s power level is greater than or equal to the kick
	// level, and the target user’s power level is less than the sender’s power
	// level, allow.
	//
	// Otherwise, reject.
	if sender_power_level >= kick_power_level && target_user_power_level < sender_power_level {
		Ok(())
	} else {
		Err!("sender does not have enough power to kick target user")
	}
}

/// Check whether the given event passes the `m.room.member` authorization rules
/// with a membership of `ban`.
#[tracing::instrument(level = "trace", skip_all)]
async fn check_room_member_ban<Fetch, Fut, Pdu>(
	room_member_event: &RoomMemberEvent<Pdu>,
	target_user: &UserId,
	rules: &AuthorizationRules,
	room_create_event: &RoomCreateEvent<Pdu>,
	fetch_state: &Fetch,
) -> Result
where
	Fetch: Fn(StateEventType, StateKey) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let (sender_membership, room_power_levels_event) = join(
		fetch_state.user_membership(room_member_event.sender()),
		fetch_state.room_power_levels_event(),
	)
	.await;

	// Since v1, if the sender’s current membership state is not join, reject.
	let sender_membership = sender_membership?;
	if sender_membership != MembershipState::Join {
		return Err!("cannot ban if sender is not joined");
	}

	let creators = room_create_event.creators(rules)?;

	let sender_power_level = room_power_levels_event.user_power_level(
		room_member_event.sender(),
		creators.clone(),
		rules,
	)?;

	let ban_power_level =
		room_power_levels_event.get_as_int_or_default(RoomPowerLevelsIntField::Ban, rules)?;

	let target_user_power_level =
		room_power_levels_event.user_power_level(target_user, creators, rules)?;

	// If the sender’s power level is greater than or equal to the ban level, and
	// the target user’s power level is less than the sender’s power level, allow.
	//
	// Otherwise, reject.
	if sender_power_level >= ban_power_level && target_user_power_level < sender_power_level {
		Ok(())
	} else {
		Err!("sender does not have enough power to ban target user")
	}
}

/// Check whether the given event passes the `m.room.member` authorization rules
/// with a membership of `knock`.
#[tracing::instrument(level = "trace", skip_all)]
async fn check_room_member_knock<Fetch, Fut, Pdu>(
	room_member_event: &RoomMemberEvent<Pdu>,
	target_user: &UserId,
	rules: &AuthorizationRules,
	fetch_state: &Fetch,
) -> Result
where
	Fetch: Fn(StateEventType, StateKey) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let sender = room_member_event.sender();
	let (join_rule, sender_membership) =
		join(fetch_state.join_rule(), fetch_state.user_membership(sender)).await;

	// v7-v9, if the join_rule is anything other than knock, reject.
	// Since v10, if the join_rule is anything other than knock or knock_restricted,
	// reject.
	let join_rule = join_rule?;
	if join_rule != JoinRule::Knock
		&& (rules.knock_restricted_join_rule && !matches!(join_rule, JoinRule::KnockRestricted))
	{
		return Err!(
			"join rule is not set to knock or knock_restricted, knocking is not allowed"
		);
	}

	// Since v7, if sender does not match state_key, reject.
	if room_member_event.sender() != target_user {
		return Err!("cannot make another user knock, sender does not match target user");
	}

	// Since v7, if the sender’s current membership is not ban, invite, or join,
	// allow. Otherwise, reject.
	let sender_membership = sender_membership?;
	if !matches!(
		sender_membership,
		MembershipState::Ban | MembershipState::Invite | MembershipState::Join
	) {
		Ok(())
	} else {
		Err!("cannot knock if user is banned, invited or joined")
	}
}
