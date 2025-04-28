use std::{borrow::Borrow, collections::BTreeSet};

use futures::{
	Future,
	future::{OptionFuture, join3},
};
use ruma::{
	Int, OwnedUserId, RoomVersionId, UserId,
	events::room::{
		create::RoomCreateEventContent,
		join_rules::{JoinRule, RoomJoinRulesEventContent},
		member::{MembershipState, ThirdPartyInvite},
		power_levels::RoomPowerLevelsEventContent,
		third_party_invite::RoomThirdPartyInviteEventContent,
	},
	int,
	serde::{Base64, Raw},
};
use serde::{
	Deserialize,
	de::{Error as _, IgnoredAny},
};
use serde_json::{from_str as from_json_str, value::RawValue as RawJsonValue};

use super::{
	Error, Event, Result, StateEventType, StateKey, TimelineEventType,
	power_levels::{
		deserialize_power_levels, deserialize_power_levels_content_fields,
		deserialize_power_levels_content_invite, deserialize_power_levels_content_redact,
	},
	room_version::RoomVersion,
};
use crate::{debug, error, trace, warn};

// FIXME: field extracting could be bundled for `content`
#[derive(Deserialize)]
struct GetMembership {
	membership: MembershipState,
}

#[derive(Deserialize)]
struct RoomMemberContentFields {
	membership: Option<Raw<MembershipState>>,
	join_authorised_via_users_server: Option<Raw<OwnedUserId>>,
}

/// For the given event `kind` what are the relevant auth events that are needed
/// to authenticate this `content`.
///
/// # Errors
///
/// This function will return an error if the supplied `content` is not a JSON
/// object.
pub fn auth_types_for_event(
	kind: &TimelineEventType,
	sender: &UserId,
	state_key: Option<&str>,
	content: &RawJsonValue,
) -> serde_json::Result<Vec<(StateEventType, StateKey)>> {
	if kind == &TimelineEventType::RoomCreate {
		return Ok(vec![]);
	}

	let mut auth_types = vec![
		(StateEventType::RoomPowerLevels, StateKey::new()),
		(StateEventType::RoomMember, sender.as_str().into()),
		(StateEventType::RoomCreate, StateKey::new()),
	];

	if kind == &TimelineEventType::RoomMember {
		#[derive(Deserialize)]
		struct RoomMemberContentFields {
			membership: Option<Raw<MembershipState>>,
			third_party_invite: Option<Raw<ThirdPartyInvite>>,
			join_authorised_via_users_server: Option<Raw<OwnedUserId>>,
		}

		if let Some(state_key) = state_key {
			let content: RoomMemberContentFields = from_json_str(content.get())?;

			if let Some(Ok(membership)) = content.membership.map(|m| m.deserialize()) {
				if [MembershipState::Join, MembershipState::Invite, MembershipState::Knock]
					.contains(&membership)
				{
					let key = (StateEventType::RoomJoinRules, StateKey::new());
					if !auth_types.contains(&key) {
						auth_types.push(key);
					}

					if let Some(Ok(u)) = content
						.join_authorised_via_users_server
						.map(|m| m.deserialize())
					{
						let key = (StateEventType::RoomMember, u.as_str().into());
						if !auth_types.contains(&key) {
							auth_types.push(key);
						}
					}
				}

				let key = (StateEventType::RoomMember, state_key.into());
				if !auth_types.contains(&key) {
					auth_types.push(key);
				}

				if membership == MembershipState::Invite {
					if let Some(Ok(t_id)) = content
						.third_party_invite
						.map(|t| t.deserialize())
					{
						let key =
							(StateEventType::RoomThirdPartyInvite, t_id.signed.token.into());
						if !auth_types.contains(&key) {
							auth_types.push(key);
						}
					}
				}
			}
		}
	}

	Ok(auth_types)
}

/// Authenticate the incoming `event`.
///
/// The steps of authentication are:
///
/// * check that the event is being authenticated for the correct room
/// * then there are checks for specific event types
///
/// The `fetch_state` closure should gather state from a state snapshot. We need
/// to know if the event passes auth against some state not a recursive
/// collection of auth_events fields.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		event_id = incoming_event.event_id().borrow().as_str()
	)
)]
pub async fn auth_check<F, Fut, Fetched, Incoming>(
	room_version: &RoomVersion,
	incoming_event: &Incoming,
	current_third_party_invite: Option<&Incoming>,
	fetch_state: F,
) -> Result<bool, Error>
where
	F: Fn(&StateEventType, &str) -> Fut + Send,
	Fut: Future<Output = Option<Fetched>> + Send,
	Fetched: Event + Send,
	Incoming: Event + Send + Sync,
{
	debug!(
		"auth_check beginning for {} ({})",
		incoming_event.event_id(),
		incoming_event.event_type()
	);

	// [synapse] check that all the events are in the same room as `incoming_event`

	// [synapse] do_sig_check check the event has valid signatures for member events

	// TODO do_size_check is false when called by `iterative_auth_check`
	// do_size_check is also mostly accomplished by ruma with the exception of
	// checking event_type, state_key, and json are below a certain size (255 and
	// 65_536 respectively)

	let sender = incoming_event.sender();

	// Implementation of https://spec.matrix.org/latest/rooms/v1/#authorization-rules
	//
	// 1. If type is m.room.create:
	if *incoming_event.event_type() == TimelineEventType::RoomCreate {
		#[derive(Deserialize)]
		struct RoomCreateContentFields {
			room_version: Option<Raw<RoomVersionId>>,
			creator: Option<Raw<IgnoredAny>>,
		}

		debug!("start m.room.create check");

		// If it has any previous events, reject
		if incoming_event.prev_events().next().is_some() {
			warn!("the room creation event had previous events");
			return Ok(false);
		}

		// If the domain of the room_id does not match the domain of the sender, reject
		let Some(room_id_server_name) = incoming_event.room_id().server_name() else {
			warn!("room ID has no servername");
			return Ok(false);
		};

		if room_id_server_name != sender.server_name() {
			warn!("servername of room ID does not match servername of sender");
			return Ok(false);
		}

		// If content.room_version is present and is not a recognized version, reject
		let content: RoomCreateContentFields = from_json_str(incoming_event.content().get())?;
		if content
			.room_version
			.is_some_and(|v| v.deserialize().is_err())
		{
			warn!("invalid room version found in m.room.create event");
			return Ok(false);
		}

		if !room_version.use_room_create_sender {
			// If content has no creator field, reject
			if content.creator.is_none() {
				warn!("no creator field found in m.room.create content");
				return Ok(false);
			}
		}

		debug!("m.room.create event was allowed");
		return Ok(true);
	}

	/*
	// TODO: In the past this code caused problems federating with synapse, maybe this has been
	// resolved already. Needs testing.
	//
	// 2. Reject if auth_events
	// a. auth_events cannot have duplicate keys since it's a BTree
	// b. All entries are valid auth events according to spec
	let expected_auth = auth_types_for_event(
		incoming_event.kind,
		sender,
		incoming_event.state_key,
		incoming_event.content().clone(),
	);

	dbg!(&expected_auth);

	for ev_key in auth_events.keys() {
		// (b)
		if !expected_auth.contains(ev_key) {
			warn!("auth_events contained invalid auth event");
			return Ok(false);
		}
	}
	*/

	let (room_create_event, power_levels_event, sender_member_event) = join3(
		fetch_state(&StateEventType::RoomCreate, ""),
		fetch_state(&StateEventType::RoomPowerLevels, ""),
		fetch_state(&StateEventType::RoomMember, sender.as_str()),
	)
	.await;

	let room_create_event = match room_create_event {
		| None => {
			warn!("no m.room.create event in auth chain");
			return Ok(false);
		},
		| Some(e) => e,
	};

	// 3. If event does not have m.room.create in auth_events reject
	if !incoming_event
		.auth_events()
		.any(|id| id.borrow() == room_create_event.event_id().borrow())
	{
		warn!("no m.room.create event in auth events");
		return Ok(false);
	}

	// If the create event content has the field m.federate set to false and the
	// sender domain of the event does not match the sender domain of the create
	// event, reject.
	#[derive(Deserialize)]
	#[allow(clippy::items_after_statements)]
	struct RoomCreateContentFederate {
		#[serde(rename = "m.federate", default = "ruma::serde::default_true")]
		federate: bool,
	}
	let room_create_content: RoomCreateContentFederate =
		from_json_str(room_create_event.content().get())?;
	if !room_create_content.federate
		&& room_create_event.sender().server_name() != incoming_event.sender().server_name()
	{
		warn!(
			"room is not federated and event's sender domain does not match create event's \
			 sender domain"
		);
		return Ok(false);
	}

	// Only in some room versions 6 and below
	if room_version.special_case_aliases_auth {
		// 4. If type is m.room.aliases
		if *incoming_event.event_type() == TimelineEventType::RoomAliases {
			debug!("starting m.room.aliases check");

			// If sender's domain doesn't matches state_key, reject
			if incoming_event.state_key() != Some(sender.server_name().as_str()) {
				warn!("state_key does not match sender");
				return Ok(false);
			}

			debug!("m.room.aliases event was allowed");
			return Ok(true);
		}
	}

	// If type is m.room.member
	if *incoming_event.event_type() == TimelineEventType::RoomMember {
		debug!("starting m.room.member check");
		let state_key = match incoming_event.state_key() {
			| None => {
				warn!("no statekey in member event");
				return Ok(false);
			},
			| Some(s) => s,
		};

		let content: RoomMemberContentFields = from_json_str(incoming_event.content().get())?;
		if content
			.membership
			.as_ref()
			.and_then(|m| m.deserialize().ok())
			.is_none()
		{
			warn!("no valid membership field found for m.room.member event content");
			return Ok(false);
		}

		let target_user =
			<&UserId>::try_from(state_key).map_err(|e| Error::InvalidPdu(format!("{e}")))?;

		let user_for_join_auth = content
			.join_authorised_via_users_server
			.as_ref()
			.and_then(|u| u.deserialize().ok());

		let user_for_join_auth_event: OptionFuture<_> = user_for_join_auth
			.as_ref()
			.map(|auth_user| fetch_state(&StateEventType::RoomMember, auth_user.as_str()))
			.into();

		let target_user_member_event =
			fetch_state(&StateEventType::RoomMember, target_user.as_str());

		let join_rules_event = fetch_state(&StateEventType::RoomJoinRules, "");

		let (join_rules_event, target_user_member_event, user_for_join_auth_event) =
			join3(join_rules_event, target_user_member_event, user_for_join_auth_event).await;

		let user_for_join_auth_membership = user_for_join_auth_event
			.and_then(|mem| from_json_str::<GetMembership>(mem?.content().get()).ok())
			.map_or(MembershipState::Leave, |mem| mem.membership);

		if !valid_membership_change(
			room_version,
			target_user,
			target_user_member_event.as_ref(),
			sender,
			sender_member_event.as_ref(),
			incoming_event,
			current_third_party_invite,
			power_levels_event.as_ref(),
			join_rules_event.as_ref(),
			user_for_join_auth.as_deref(),
			&user_for_join_auth_membership,
			&room_create_event,
		)? {
			return Ok(false);
		}

		debug!("m.room.member event was allowed");
		return Ok(true);
	}

	// If the sender's current membership state is not join, reject
	#[allow(clippy::manual_let_else)]
	let sender_member_event = match sender_member_event {
		| Some(mem) => mem,
		| None => {
			warn!("sender not found in room");
			return Ok(false);
		},
	};

	let sender_membership_event_content: RoomMemberContentFields =
		from_json_str(sender_member_event.content().get())?;
	let membership_state = sender_membership_event_content
		.membership
		.expect("we should test before that this field exists")
		.deserialize()?;

	if !matches!(membership_state, MembershipState::Join) {
		warn!("sender's membership is not join");
		return Ok(false);
	}

	// If type is m.room.third_party_invite
	let sender_power_level = match &power_levels_event {
		| Some(pl) => {
			let content =
				deserialize_power_levels_content_fields(pl.content().get(), room_version)?;
			match content.get_user_power(sender) {
				| Some(level) => *level,
				| _ => content.users_default,
			}
		},
		| _ => {
			// If no power level event found the creator gets 100 everyone else gets 0
			let is_creator = if room_version.use_room_create_sender {
				room_create_event.sender() == sender
			} else {
				#[allow(deprecated)]
				from_json_str::<RoomCreateEventContent>(room_create_event.content().get())
					.is_ok_and(|create| create.creator.unwrap() == *sender)
			};

			if is_creator { int!(100) } else { int!(0) }
		},
	};

	// Allow if and only if sender's current power level is greater than
	// or equal to the invite level
	if *incoming_event.event_type() == TimelineEventType::RoomThirdPartyInvite {
		let invite_level = match &power_levels_event {
			| Some(power_levels) => {
				deserialize_power_levels_content_invite(
					power_levels.content().get(),
					room_version,
				)?
				.invite
			},
			| None => int!(0),
		};

		if sender_power_level < invite_level {
			warn!("sender's cannot send invites in this room");
			return Ok(false);
		}

		debug!("m.room.third_party_invite event was allowed");
		return Ok(true);
	}

	// If the event type's required power level is greater than the sender's power
	// level, reject If the event has a state_key that starts with an @ and does
	// not match the sender, reject.
	if !can_send_event(incoming_event, power_levels_event.as_ref(), sender_power_level) {
		warn!("user cannot send event");
		return Ok(false);
	}

	// If type is m.room.power_levels
	if *incoming_event.event_type() == TimelineEventType::RoomPowerLevels {
		debug!("starting m.room.power_levels check");

		match check_power_levels(
			room_version,
			incoming_event,
			power_levels_event.as_ref(),
			sender_power_level,
		) {
			| Some(required_pwr_lvl) => {
				if !required_pwr_lvl {
					warn!("m.room.power_levels was not allowed");
					return Ok(false);
				}
			},
			| _ => {
				warn!("m.room.power_levels was not allowed");
				return Ok(false);
			},
		}
		debug!("m.room.power_levels event allowed");
	}

	// Room version 3: Redaction events are always accepted (provided the event is
	// allowed by `events` and `events_default` in the power levels). However,
	// servers should not apply or send redaction's to clients until both the
	// redaction event and original event have been seen, and are valid. Servers
	// should only apply redaction's to events where the sender's domains match, or
	// the sender of the redaction has the appropriate permissions per the
	// power levels.

	if room_version.extra_redaction_checks
		&& *incoming_event.event_type() == TimelineEventType::RoomRedaction
	{
		let redact_level = match power_levels_event {
			| Some(pl) => {
				deserialize_power_levels_content_redact(pl.content().get(), room_version)?.redact
			},
			| None => int!(50),
		};

		if !check_redaction(room_version, incoming_event, sender_power_level, redact_level)? {
			return Ok(false);
		}
	}

	debug!("allowing event passed all checks");
	Ok(true)
}

// TODO deserializing the member, power, join_rules event contents is done in
// conduit just before this is called. Could they be passed in?
/// Does the user who sent this member event have required power levels to do
/// so.
///
/// * `user` - Information about the membership event and user making the
///   request.
/// * `auth_events` - The set of auth events that relate to a membership event.
///
/// This is generated by calling `auth_types_for_event` with the membership
/// event and the current State.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::cognitive_complexity)]
fn valid_membership_change(
	room_version: &RoomVersion,
	target_user: &UserId,
	target_user_membership_event: Option<&impl Event>,
	sender: &UserId,
	sender_membership_event: Option<&impl Event>,
	current_event: impl Event,
	current_third_party_invite: Option<&impl Event>,
	power_levels_event: Option<&impl Event>,
	join_rules_event: Option<&impl Event>,
	user_for_join_auth: Option<&UserId>,
	user_for_join_auth_membership: &MembershipState,
	create_room: &impl Event,
) -> Result<bool> {
	#[derive(Deserialize)]
	struct GetThirdPartyInvite {
		third_party_invite: Option<Raw<ThirdPartyInvite>>,
	}
	let content = current_event.content();

	let target_membership = from_json_str::<GetMembership>(content.get())?.membership;
	let third_party_invite =
		from_json_str::<GetThirdPartyInvite>(content.get())?.third_party_invite;

	let sender_membership = match &sender_membership_event {
		| Some(pdu) => from_json_str::<GetMembership>(pdu.content().get())?.membership,
		| None => MembershipState::Leave,
	};
	let sender_is_joined = sender_membership == MembershipState::Join;

	let target_user_current_membership = match &target_user_membership_event {
		| Some(pdu) => from_json_str::<GetMembership>(pdu.content().get())?.membership,
		| None => MembershipState::Leave,
	};

	let power_levels: RoomPowerLevelsEventContent = match &power_levels_event {
		| Some(ev) => from_json_str(ev.content().get())?,
		| None => RoomPowerLevelsEventContent::default(),
	};

	let sender_power = power_levels
		.users
		.get(sender)
		.or_else(|| sender_is_joined.then_some(&power_levels.users_default));

	let target_power = power_levels.users.get(target_user).or_else(|| {
		(target_membership == MembershipState::Join).then_some(&power_levels.users_default)
	});

	let mut join_rules = JoinRule::Invite;
	if let Some(jr) = &join_rules_event {
		join_rules = from_json_str::<RoomJoinRulesEventContent>(jr.content().get())?.join_rule;
	}

	let power_levels_event_id = power_levels_event.as_ref().map(Event::event_id);
	let sender_membership_event_id = sender_membership_event
		.as_ref()
		.map(Event::event_id);
	let target_user_membership_event_id = target_user_membership_event
		.as_ref()
		.map(Event::event_id);

	let user_for_join_auth_is_valid = if let Some(user_for_join_auth) = user_for_join_auth {
		// Is the authorised user allowed to invite users into this room
		let (auth_user_pl, invite_level) = if let Some(pl) = &power_levels_event {
			// TODO Refactor all powerlevel parsing
			let invite =
				deserialize_power_levels_content_invite(pl.content().get(), room_version)?.invite;

			let content =
				deserialize_power_levels_content_fields(pl.content().get(), room_version)?;
			let user_pl = match content.get_user_power(user_for_join_auth) {
				| Some(level) => *level,
				| _ => content.users_default,
			};

			(user_pl, invite)
		} else {
			(int!(0), int!(0))
		};
		(user_for_join_auth_membership == &MembershipState::Join)
			&& (auth_user_pl >= invite_level)
	} else {
		// No auth user was given
		false
	};

	Ok(match target_membership {
		| MembershipState::Join => {
			// 1. If the only previous event is an m.room.create and the state_key is the
			//    creator,
			// allow
			let mut prev_events = current_event.prev_events();

			let prev_event_is_create_event = prev_events
				.next()
				.is_some_and(|event_id| event_id.borrow() == create_room.event_id().borrow());
			let no_more_prev_events = prev_events.next().is_none();

			if prev_event_is_create_event && no_more_prev_events {
				let is_creator = if room_version.use_room_create_sender {
					let creator = create_room.sender();

					creator == sender && creator == target_user
				} else {
					#[allow(deprecated)]
					let creator = from_json_str::<RoomCreateEventContent>(create_room.content().get())?
						.creator
						.ok_or_else(|| serde_json::Error::missing_field("creator"))?;

					creator == sender && creator == target_user
				};

				if is_creator {
					return Ok(true);
				}
			}

			if sender != target_user {
				// If the sender does not match state_key, reject.
				warn!("Can't make other user join");
				false
			} else if target_user_current_membership == MembershipState::Ban {
				// If the sender is banned, reject.
				warn!(?target_user_membership_event_id, "Banned user can't join");
				false
			} else if (join_rules == JoinRule::Invite
                    || room_version.allow_knocking && join_rules == JoinRule::Knock)
                // If the join_rule is invite then allow if membership state is invite or join
                    && (target_user_current_membership == MembershipState::Join
                        || target_user_current_membership == MembershipState::Invite)
			{
				true
			} else if room_version.restricted_join_rules
				&& matches!(join_rules, JoinRule::Restricted(_))
				|| room_version.knock_restricted_join_rule
					&& matches!(join_rules, JoinRule::KnockRestricted(_))
			{
				// If the join_rule is restricted or knock_restricted
				if matches!(
					target_user_current_membership,
					MembershipState::Invite | MembershipState::Join
				) {
					// If membership state is join or invite, allow.
					true
				} else {
					// If the join_authorised_via_users_server key in content is not a user with
					// sufficient permission to invite other users, reject.
					// Otherwise, allow.
					user_for_join_auth_is_valid
				}
			} else {
				// If the join_rule is public, allow.
				// Otherwise, reject.
				join_rules == JoinRule::Public
			}
		},
		| MembershipState::Invite => {
			// If content has third_party_invite key
			match third_party_invite.and_then(|i| i.deserialize().ok()) {
				| Some(tp_id) => {
					if target_user_current_membership == MembershipState::Ban {
						warn!(?target_user_membership_event_id, "Can't invite banned user");
						false
					} else {
						let allow = verify_third_party_invite(
							Some(target_user),
							sender,
							&tp_id,
							current_third_party_invite,
						);
						if !allow {
							warn!("Third party invite invalid");
						}
						allow
					}
				},
				| _ => {
					if !sender_is_joined
						|| target_user_current_membership == MembershipState::Join
						|| target_user_current_membership == MembershipState::Ban
					{
						warn!(
							?target_user_membership_event_id,
							?sender_membership_event_id,
							"Can't invite user if sender not joined or the user is currently \
							 joined or banned",
						);
						false
					} else {
						let allow = sender_power
							.filter(|&p| p >= &power_levels.invite)
							.is_some();
						if !allow {
							warn!(
								?target_user_membership_event_id,
								?power_levels_event_id,
								"User does not have enough power to invite",
							);
						}
						allow
					}
				},
			}
		},
		| MembershipState::Leave => {
			if sender == target_user {
				let allow = target_user_current_membership == MembershipState::Join
					|| target_user_current_membership == MembershipState::Invite
					|| target_user_current_membership == MembershipState::Knock;
				if !allow {
					warn!(
						?target_user_membership_event_id,
						?target_user_current_membership,
						"Can't leave if sender is not already invited, knocked, or joined"
					);
				}
				allow
			} else if !sender_is_joined
				|| target_user_current_membership == MembershipState::Ban
					&& sender_power
						.filter(|&p| p < &power_levels.ban)
						.is_some()
			{
				warn!(
					?target_user_membership_event_id,
					?sender_membership_event_id,
					"Can't kick if sender not joined or user is already banned",
				);
				false
			} else {
				let allow = sender_power
					.filter(|&p| p >= &power_levels.kick)
					.is_some() && target_power < sender_power;
				if !allow {
					warn!(
						?target_user_membership_event_id,
						?power_levels_event_id,
						"User does not have enough power to kick",
					);
				}
				allow
			}
		},
		| MembershipState::Ban => {
			if !sender_is_joined {
				warn!(?sender_membership_event_id, "Can't ban user if sender is not joined");
				false
			} else {
				let allow = sender_power
					.filter(|&p| p >= &power_levels.ban)
					.is_some() && target_power < sender_power;
				if !allow {
					warn!(
						?target_user_membership_event_id,
						?power_levels_event_id,
						"User does not have enough power to ban",
					);
				}
				allow
			}
		},
		| MembershipState::Knock if room_version.allow_knocking => {
			// 1. If the `join_rule` is anything other than `knock` or `knock_restricted`,
			//    reject.
			if !matches!(join_rules, JoinRule::KnockRestricted(_) | JoinRule::Knock) {
				warn!(
					"Join rule is not set to knock or knock_restricted, knocking is not allowed"
				);
				false
			} else if matches!(join_rules, JoinRule::KnockRestricted(_))
				&& !room_version.knock_restricted_join_rule
			{
				// 2. If the `join_rule` is `knock_restricted`, but the room does not support
				//    `knock_restricted`, reject.
				warn!(
					"Join rule is set to knock_restricted but room version does not support \
					 knock_restricted, knocking is not allowed"
				);
				false
			} else if sender != target_user {
				// 3. If `sender` does not match `state_key`, reject.
				warn!(
					?sender,
					?target_user,
					"Can't make another user knock, sender did not match target"
				);
				false
			} else if matches!(
				sender_membership,
				MembershipState::Ban | MembershipState::Invite | MembershipState::Join
			) {
				// 4. If the `sender`'s current membership is not `ban`, `invite`, or `join`,
				//    allow.
				// 5. Otherwise, reject.
				warn!(
					?target_user_membership_event_id,
					"Knocking with a membership state of ban, invite or join is invalid",
				);
				false
			} else {
				true
			}
		},
		| _ => {
			warn!("Unknown membership transition");
			false
		},
	})
}

/// Is the user allowed to send a specific event based on the rooms power
/// levels.
///
/// Does the event have the correct userId as its state_key if it's not the ""
/// state_key.
fn can_send_event(event: impl Event, ple: Option<impl Event>, user_level: Int) -> bool {
	let event_type_power_level = get_send_level(event.event_type(), event.state_key(), ple);

	debug!(
		required_level = i64::from(event_type_power_level),
		user_level = i64::from(user_level),
		state_key = ?event.state_key(),
		"permissions factors",
	);

	if user_level < event_type_power_level {
		return false;
	}

	if event
		.state_key()
		.is_some_and(|k| k.starts_with('@'))
		&& event.state_key() != Some(event.sender().as_str())
	{
		return false; // permission required to post in this room
	}

	true
}

/// Confirm that the event sender has the required power levels.
fn check_power_levels(
	room_version: &RoomVersion,
	power_event: impl Event,
	previous_power_event: Option<impl Event>,
	user_level: Int,
) -> Option<bool> {
	match power_event.state_key() {
		| Some("") => {},
		| Some(key) => {
			error!(state_key = key, "m.room.power_levels event has non-empty state key");
			return None;
		},
		| None => {
			error!("check_power_levels requires an m.room.power_levels *state* event argument");
			return None;
		},
	}

	// - If any of the keys users_default, events_default, state_default, ban,
	//   redact, kick, or invite in content are present and not an integer, reject.
	// - If either of the keys events or notifications in content are present and
	//   not a dictionary with values that are integers, reject.
	// - If users key in content is not a dictionary with keys that are valid user
	//   IDs with values that are integers, reject.
	let user_content: RoomPowerLevelsEventContent =
		deserialize_power_levels(power_event.content().get(), room_version)?;

	// Validation of users is done in Ruma, synapse for loops validating user_ids
	// and integers here
	debug!("validation of power event finished");

	#[allow(clippy::manual_let_else)]
	let current_state = match previous_power_event {
		| Some(current_state) => current_state,
		// If there is no previous m.room.power_levels event in the room, allow
		| None => return Some(true),
	};

	let current_content: RoomPowerLevelsEventContent =
		deserialize_power_levels(current_state.content().get(), room_version)?;

	let mut user_levels_to_check = BTreeSet::new();
	let old_list = &current_content.users;
	let user_list = &user_content.users;
	for user in old_list.keys().chain(user_list.keys()) {
		let user: &UserId = user;
		user_levels_to_check.insert(user);
	}

	trace!(set = ?user_levels_to_check, "user levels to check");

	let mut event_levels_to_check = BTreeSet::new();
	let old_list = &current_content.events;
	let new_list = &user_content.events;
	for ev_id in old_list.keys().chain(new_list.keys()) {
		event_levels_to_check.insert(ev_id);
	}

	trace!(set = ?event_levels_to_check, "event levels to check");

	let old_state = &current_content;
	let new_state = &user_content;

	// synapse does not have to split up these checks since we can't combine UserIds
	// and EventTypes we do 2 loops

	// UserId loop
	for user in user_levels_to_check {
		let old_level = old_state.users.get(user);
		let new_level = new_state.users.get(user);
		if old_level.is_some() && new_level.is_some() && old_level == new_level {
			continue;
		}

		// If the current value is equal to the sender's current power level, reject
		if user != power_event.sender() && old_level == Some(&user_level) {
			warn!("m.room.power_level cannot remove ops == to own");
			return Some(false); // cannot remove ops level == to own
		}

		// If the current value is higher than the sender's current power level, reject
		// If the new value is higher than the sender's current power level, reject
		let old_level_too_big = old_level > Some(&user_level);
		let new_level_too_big = new_level > Some(&user_level);
		if old_level_too_big || new_level_too_big {
			warn!("m.room.power_level failed to add ops > than own");
			return Some(false); // cannot add ops greater than own
		}
	}

	// EventType loop
	for ev_type in event_levels_to_check {
		let old_level = old_state.events.get(ev_type);
		let new_level = new_state.events.get(ev_type);
		if old_level.is_some() && new_level.is_some() && old_level == new_level {
			continue;
		}

		// If the current value is higher than the sender's current power level, reject
		// If the new value is higher than the sender's current power level, reject
		let old_level_too_big = old_level > Some(&user_level);
		let new_level_too_big = new_level > Some(&user_level);
		if old_level_too_big || new_level_too_big {
			warn!("m.room.power_level failed to add ops > than own");
			return Some(false); // cannot add ops greater than own
		}
	}

	// Notifications, currently there is only @room
	if room_version.limit_notifications_power_levels {
		let old_level = old_state.notifications.room;
		let new_level = new_state.notifications.room;
		if old_level != new_level {
			// If the current value is higher than the sender's current power level, reject
			// If the new value is higher than the sender's current power level, reject
			let old_level_too_big = old_level > user_level;
			let new_level_too_big = new_level > user_level;
			if old_level_too_big || new_level_too_big {
				warn!("m.room.power_level failed to add ops > than own");
				return Some(false); // cannot add ops greater than own
			}
		}
	}

	let levels = [
		"users_default",
		"events_default",
		"state_default",
		"ban",
		"redact",
		"kick",
		"invite",
	];
	let old_state = serde_json::to_value(old_state).unwrap();
	let new_state = serde_json::to_value(new_state).unwrap();
	for lvl_name in &levels {
		if let Some((old_lvl, new_lvl)) = get_deserialize_levels(&old_state, &new_state, lvl_name)
		{
			let old_level_too_big = old_lvl > user_level;
			let new_level_too_big = new_lvl > user_level;

			if old_level_too_big || new_level_too_big {
				warn!("cannot add ops > than own");
				return Some(false);
			}
		}
	}

	Some(true)
}

fn get_deserialize_levels(
	old: &serde_json::Value,
	new: &serde_json::Value,
	name: &str,
) -> Option<(Int, Int)> {
	Some((
		serde_json::from_value(old.get(name)?.clone()).ok()?,
		serde_json::from_value(new.get(name)?.clone()).ok()?,
	))
}

/// Does the event redacting come from a user with enough power to redact the
/// given event.
fn check_redaction(
	_room_version: &RoomVersion,
	redaction_event: impl Event,
	user_level: Int,
	redact_level: Int,
) -> Result<bool> {
	if user_level >= redact_level {
		debug!("redaction allowed via power levels");
		return Ok(true);
	}

	// If the domain of the event_id of the event being redacted is the same as the
	// domain of the event_id of the m.room.redaction, allow
	if redaction_event.event_id().borrow().server_name()
		== redaction_event
			.redacts()
			.as_ref()
			.and_then(|&id| id.borrow().server_name())
	{
		debug!("redaction event allowed via room version 1 rules");
		return Ok(true);
	}

	Ok(false)
}

/// Helper function to fetch the power level needed to send an event of type
/// `e_type` based on the rooms "m.room.power_level" event.
fn get_send_level(
	e_type: &TimelineEventType,
	state_key: Option<&str>,
	power_lvl: Option<impl Event>,
) -> Int {
	power_lvl
		.and_then(|ple| {
			from_json_str::<RoomPowerLevelsEventContent>(ple.content().get())
				.map(|content| {
					content
						.events
						.get(e_type)
						.copied()
						.unwrap_or_else(|| {
							if state_key.is_some() {
								content.state_default
							} else {
								content.events_default
							}
						})
				})
				.ok()
		})
		.unwrap_or_else(|| if state_key.is_some() { int!(50) } else { int!(0) })
}

fn verify_third_party_invite(
	target_user: Option<&UserId>,
	sender: &UserId,
	tp_id: &ThirdPartyInvite,
	current_third_party_invite: Option<impl Event>,
) -> bool {
	// 1. Check for user being banned happens before this is called
	// checking for mxid and token keys is done by ruma when deserializing

	// The state key must match the invitee
	if target_user != Some(&tp_id.signed.mxid) {
		return false;
	}

	// If there is no m.room.third_party_invite event in the current room state with
	// state_key matching token, reject
	#[allow(clippy::manual_let_else)]
	let current_tpid = match current_third_party_invite {
		| Some(id) => id,
		| None => return false,
	};

	if current_tpid.state_key() != Some(&tp_id.signed.token) {
		return false;
	}

	if sender != current_tpid.sender() {
		return false;
	}

	// If any signature in signed matches any public key in the
	// m.room.third_party_invite event, allow
	#[allow(clippy::manual_let_else)]
	let tpid_ev =
		match from_json_str::<RoomThirdPartyInviteEventContent>(current_tpid.content().get()) {
			| Ok(ev) => ev,
			| Err(_) => return false,
		};

	#[allow(clippy::manual_let_else)]
	let decoded_invite_token = match Base64::parse(&tp_id.signed.token) {
		| Ok(tok) => tok,
		// FIXME: Log a warning?
		| Err(_) => return false,
	};

	// A list of public keys in the public_keys field
	for key in tpid_ev.public_keys.unwrap_or_default() {
		if key.public_key == decoded_invite_token {
			return true;
		}
	}

	// A single public key in the public_key field
	tpid_ev.public_key == decoded_invite_token
}

#[cfg(test)]
mod tests {
	use ruma::events::{
		StateEventType, TimelineEventType,
		room::{
			join_rules::{
				AllowRule, JoinRule, Restricted, RoomJoinRulesEventContent, RoomMembership,
			},
			member::{MembershipState, RoomMemberEventContent},
		},
	};
	use serde_json::value::to_raw_value as to_raw_json_value;

	use crate::state_res::{
		Event, EventTypeExt, RoomVersion, StateMap,
		event_auth::valid_membership_change,
		test_utils::{
			INITIAL_EVENTS, INITIAL_EVENTS_CREATE_ROOM, PduEvent, alice, charlie, ella, event_id,
			member_content_ban, member_content_join, room_id, to_pdu_event,
		},
	};

	#[test]
	fn test_ban_pass() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);
		let events = INITIAL_EVENTS();

		let auth_events = events
			.values()
			.map(|ev| {
				(
					ev.event_type()
						.with_state_key(ev.state_key().unwrap()),
					ev.clone(),
				)
			})
			.collect::<StateMap<_>>();

		let requester = to_pdu_event(
			"HELLO",
			alice(),
			TimelineEventType::RoomMember,
			Some(charlie().as_str()),
			member_content_ban(),
			&[],
			&["IMC"],
		);

		let fetch_state = |ty, key| auth_events.get(&(ty, key)).cloned();
		let target_user = charlie();
		let sender = alice();

		assert!(
			valid_membership_change(
				&RoomVersion::V6,
				target_user,
				fetch_state(StateEventType::RoomMember, target_user.as_str().into()).as_ref(),
				sender,
				fetch_state(StateEventType::RoomMember, sender.as_str().into()).as_ref(),
				&requester,
				None::<&PduEvent>,
				fetch_state(StateEventType::RoomPowerLevels, "".into()).as_ref(),
				fetch_state(StateEventType::RoomJoinRules, "".into()).as_ref(),
				None,
				&MembershipState::Leave,
				&fetch_state(StateEventType::RoomCreate, "".into()).unwrap(),
			)
			.unwrap()
		);
	}

	#[test]
	fn test_join_non_creator() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);
		let events = INITIAL_EVENTS_CREATE_ROOM();

		let auth_events = events
			.values()
			.map(|ev| {
				(
					ev.event_type()
						.with_state_key(ev.state_key().unwrap()),
					ev.clone(),
				)
			})
			.collect::<StateMap<_>>();

		let requester = to_pdu_event(
			"HELLO",
			charlie(),
			TimelineEventType::RoomMember,
			Some(charlie().as_str()),
			member_content_join(),
			&["CREATE"],
			&["CREATE"],
		);

		let fetch_state = |ty, key| auth_events.get(&(ty, key)).cloned();
		let target_user = charlie();
		let sender = charlie();

		assert!(
			!valid_membership_change(
				&RoomVersion::V6,
				target_user,
				fetch_state(StateEventType::RoomMember, target_user.as_str().into()).as_ref(),
				sender,
				fetch_state(StateEventType::RoomMember, sender.as_str().into()).as_ref(),
				&requester,
				None::<&PduEvent>,
				fetch_state(StateEventType::RoomPowerLevels, "".into()).as_ref(),
				fetch_state(StateEventType::RoomJoinRules, "".into()).as_ref(),
				None,
				&MembershipState::Leave,
				&fetch_state(StateEventType::RoomCreate, "".into()).unwrap(),
			)
			.unwrap()
		);
	}

	#[test]
	fn test_join_creator() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);
		let events = INITIAL_EVENTS_CREATE_ROOM();

		let auth_events = events
			.values()
			.map(|ev| {
				(
					ev.event_type()
						.with_state_key(ev.state_key().unwrap()),
					ev.clone(),
				)
			})
			.collect::<StateMap<_>>();

		let requester = to_pdu_event(
			"HELLO",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&["CREATE"],
			&["CREATE"],
		);

		let fetch_state = |ty, key| auth_events.get(&(ty, key)).cloned();
		let target_user = alice();
		let sender = alice();

		assert!(
			valid_membership_change(
				&RoomVersion::V6,
				target_user,
				fetch_state(StateEventType::RoomMember, target_user.as_str().into()).as_ref(),
				sender,
				fetch_state(StateEventType::RoomMember, sender.as_str().into()).as_ref(),
				&requester,
				None::<&PduEvent>,
				fetch_state(StateEventType::RoomPowerLevels, "".into()).as_ref(),
				fetch_state(StateEventType::RoomJoinRules, "".into()).as_ref(),
				None,
				&MembershipState::Leave,
				&fetch_state(StateEventType::RoomCreate, "".into()).unwrap(),
			)
			.unwrap()
		);
	}

	#[test]
	fn test_ban_fail() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);
		let events = INITIAL_EVENTS();

		let auth_events = events
			.values()
			.map(|ev| {
				(
					ev.event_type()
						.with_state_key(ev.state_key().unwrap()),
					ev.clone(),
				)
			})
			.collect::<StateMap<_>>();

		let requester = to_pdu_event(
			"HELLO",
			charlie(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_ban(),
			&[],
			&["IMC"],
		);

		let fetch_state = |ty, key| auth_events.get(&(ty, key)).cloned();
		let target_user = alice();
		let sender = charlie();

		assert!(
			!valid_membership_change(
				&RoomVersion::V6,
				target_user,
				fetch_state(StateEventType::RoomMember, target_user.as_str().into()).as_ref(),
				sender,
				fetch_state(StateEventType::RoomMember, sender.as_str().into()).as_ref(),
				&requester,
				None::<&PduEvent>,
				fetch_state(StateEventType::RoomPowerLevels, "".into()).as_ref(),
				fetch_state(StateEventType::RoomJoinRules, "".into()).as_ref(),
				None,
				&MembershipState::Leave,
				&fetch_state(StateEventType::RoomCreate, "".into()).unwrap(),
			)
			.unwrap()
		);
	}

	#[test]
	fn test_restricted_join_rule() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);
		let mut events = INITIAL_EVENTS();
		*events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
			"IJR",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Restricted(
				Restricted::new(vec![AllowRule::RoomMembership(RoomMembership::new(
					room_id().to_owned(),
				))]),
			)))
			.unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		);

		let mut member = RoomMemberEventContent::new(MembershipState::Join);
		member.join_authorized_via_users_server = Some(alice().to_owned());

		let auth_events = events
			.values()
			.map(|ev| {
				(
					ev.event_type()
						.with_state_key(ev.state_key().unwrap()),
					ev.clone(),
				)
			})
			.collect::<StateMap<_>>();

		let requester = to_pdu_event(
			"HELLO",
			ella(),
			TimelineEventType::RoomMember,
			Some(ella().as_str()),
			to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Join)).unwrap(),
			&["CREATE", "IJR", "IPOWER", "new"],
			&["new"],
		);

		let fetch_state = |ty, key| auth_events.get(&(ty, key)).cloned();
		let target_user = ella();
		let sender = ella();

		assert!(
			valid_membership_change(
				&RoomVersion::V9,
				target_user,
				fetch_state(StateEventType::RoomMember, target_user.as_str().into()).as_ref(),
				sender,
				fetch_state(StateEventType::RoomMember, sender.as_str().into()).as_ref(),
				&requester,
				None::<&PduEvent>,
				fetch_state(StateEventType::RoomPowerLevels, "".into()).as_ref(),
				fetch_state(StateEventType::RoomJoinRules, "".into()).as_ref(),
				Some(alice()),
				&MembershipState::Join,
				&fetch_state(StateEventType::RoomCreate, "".into()).unwrap(),
			)
			.unwrap()
		);

		assert!(
			!valid_membership_change(
				&RoomVersion::V9,
				target_user,
				fetch_state(StateEventType::RoomMember, target_user.as_str().into()).as_ref(),
				sender,
				fetch_state(StateEventType::RoomMember, sender.as_str().into()).as_ref(),
				&requester,
				None::<&PduEvent>,
				fetch_state(StateEventType::RoomPowerLevels, "".into()).as_ref(),
				fetch_state(StateEventType::RoomJoinRules, "".into()).as_ref(),
				Some(ella()),
				&MembershipState::Leave,
				&fetch_state(StateEventType::RoomCreate, "".into()).unwrap(),
			)
			.unwrap()
		);
	}

	#[test]
	fn test_knock() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt()
				.with_test_writer()
				.finish(),
		);
		let mut events = INITIAL_EVENTS();
		*events.get_mut(&event_id("IJR")).unwrap() = to_pdu_event(
			"IJR",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Knock)).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["IPOWER"],
		);

		let auth_events = events
			.values()
			.map(|ev| {
				(
					ev.event_type()
						.with_state_key(ev.state_key().unwrap()),
					ev.clone(),
				)
			})
			.collect::<StateMap<_>>();

		let requester = to_pdu_event(
			"HELLO",
			ella(),
			TimelineEventType::RoomMember,
			Some(ella().as_str()),
			to_raw_json_value(&RoomMemberEventContent::new(MembershipState::Knock)).unwrap(),
			&[],
			&["IMC"],
		);

		let fetch_state = |ty, key| auth_events.get(&(ty, key)).cloned();
		let target_user = ella();
		let sender = ella();

		assert!(
			valid_membership_change(
				&RoomVersion::V7,
				target_user,
				fetch_state(StateEventType::RoomMember, target_user.as_str().into()).as_ref(),
				sender,
				fetch_state(StateEventType::RoomMember, sender.as_str().into()).as_ref(),
				&requester,
				None::<&PduEvent>,
				fetch_state(StateEventType::RoomPowerLevels, "".into()).as_ref(),
				fetch_state(StateEventType::RoomJoinRules, "".into()).as_ref(),
				None,
				&MembershipState::Leave,
				&fetch_state(StateEventType::RoomCreate, "".into()).unwrap(),
			)
			.unwrap()
		);
	}
}
