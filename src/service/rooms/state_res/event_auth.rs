mod auth_types;
mod room_member;
#[cfg(test)]
mod tests;

use futures::{
	FutureExt, TryStreamExt,
	future::{join3, try_join},
};
use ruma::{
	EventId, Int, OwnedEventId, OwnedUserId,
	api::client::error::ErrorKind::InvalidParam,
	events::{
		StateEventType, TimelineEventType,
		room::{member::MembershipState, power_levels::UserPowerLevel},
	},
	room_version_rules::{AuthorizationRules, RoomVersionRules},
};
use tuwunel_core::{
	Err, Error, Result, err,
	matrix::{Event, StateKey},
	trace,
	utils::stream::{IterStream, TryReadyExt},
};

pub use self::auth_types::{AuthTypes, auth_types_for_event};
use self::room_member::check_room_member;
#[cfg(test)]
use super::test_utils;
use super::{
	FetchStateExt, TypeStateKey, events,
	events::{
		RoomCreateEvent, RoomMemberEvent, RoomPowerLevelsEvent,
		power_levels::{self, RoomPowerLevelsEventOptionExt, RoomPowerLevelsIntField},
	},
};

#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		event_id = ?incoming_event.event_id(),
	)
)]
pub async fn auth_check<FetchEvent, EventFut, FetchState, StateFut, Pdu>(
	rules: &RoomVersionRules,
	incoming_event: &Pdu,
	fetch_event: &FetchEvent,
	fetch_state: &FetchState,
) -> Result
where
	FetchEvent: Fn(OwnedEventId) -> EventFut + Sync,
	EventFut: Future<Output = Result<Pdu>> + Send,
	FetchState: Fn(StateEventType, StateKey) -> StateFut + Sync,
	StateFut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let dependent = check_state_dependent_auth_rules(rules, incoming_event, fetch_state);

	let independent = check_state_independent_auth_rules(rules, incoming_event, fetch_event);

	match try_join(independent, dependent).await {
		| Err(e) if matches!(e, Error::Request(InvalidParam, ..)) => Err(e),
		| Err(e) => Err!(Request(Forbidden("Auth check failed: {e}"))),
		| Ok(_) => Ok(()),
	}
}

/// Check whether the incoming event passes the state-independent [authorization
/// rules] for the given room version rules.
///
/// The state-independent rules are the first few authorization rules that check
/// an incoming `m.room.create` event (which cannot have `auth_events`), and the
/// list of `auth_events` of other events.
///
/// This method only needs to be called once, when the event is received.
///
/// # Errors
///
/// If the check fails, this returns an `Err(_)` with a description of the check
/// that failed.
///
/// [authorization rules]: https://spec.matrix.org/latest/server-server-api/#authorization-rules
#[tracing::instrument(
	name = "independent",
	level = "debug",
	skip_all,
	fields(
		sender = ?incoming_event.sender(),
	)
)]
pub(super) async fn check_state_independent_auth_rules<Fetch, Fut, Pdu>(
	rules: &RoomVersionRules,
	incoming_event: &Pdu,
	fetch_event: &Fetch,
) -> Result
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	// Since v1, if type is m.room.create:
	if *incoming_event.event_type() == TimelineEventType::RoomCreate {
		let room_create_event = RoomCreateEvent::new(incoming_event.clone());
		return check_room_create(&room_create_event, &rules.authorization);
	}

	let expected_auth_types = auth_types_for_event(
		incoming_event.event_type(),
		incoming_event.sender(),
		incoming_event.state_key(),
		incoming_event.content(),
		&rules.authorization,
		false,
	)?;

	// Since v1, considering auth_events:
	let seen_auth_types = Vec::with_capacity(expected_auth_types.len());
	let seen_auth_types = incoming_event
		.auth_events()
		.try_stream()
		.and_then(async |event_id: &EventId| match fetch_event(event_id.to_owned()).await {
			| Ok(auth_event) => Ok(auth_event),
			| Err(e) if e.is_not_found() => Err!(Request(NotFound("auth event {event_id}: {e}"))),
			| Err(e) => Err(e),
		})
		.ready_try_fold(seen_auth_types, |mut seen_auth_types, auth_event| {
			let event_id = auth_event.event_id();

			// The auth event must be in the same room as the incoming event.
			if auth_event.room_id() != incoming_event.room_id() {
				return Err!("auth event {event_id} not in the same room");
			}

			let state_key = auth_event
				.state_key()
				.ok_or_else(|| err!("auth event {event_id} has no `state_key`"))?;

			let event_type = auth_event.event_type();
			let key: TypeStateKey = (event_type.to_cow_str().into(), state_key.into());

			// Since v1, if there are duplicate entries for a given type and state_key pair,
			// reject.
			if seen_auth_types.contains(&key) {
				return Err!(
					"duplicate auth event {event_id} for ({event_type}, {state_key}) pair"
				);
			}

			// Since v1, if there are entries whose type and state_key don’t match those
			// specified by the auth events selection algorithm described in the server
			// specification, reject.
			if !expected_auth_types.contains(&key) {
				return Err!(
					"unexpected auth event {event_id} with ({event_type}, {state_key}) pair"
				);
			}

			// Since v1, if there are entries which were themselves rejected under the
			// checks performed on receipt of a PDU, reject.
			if auth_event.rejected() {
				return Err!("rejected auth event {event_id}");
			}

			seen_auth_types.push(key);
			Ok(seen_auth_types)
		})
		.await?;

	// Since v1, if there is no m.room.create event among the entries, reject.
	if !rules
		.authorization
		.room_create_event_id_as_room_id
		&& !seen_auth_types
			.iter()
			.any(|(event_type, _)| *event_type == StateEventType::RoomCreate)
	{
		return Err!("no `m.room.create` event in auth events");
	}

	// Since `org.matrix.hydra.11`, the room_id must be the reference hash of an
	// accepted m.room.create event.
	if rules
		.authorization
		.room_create_event_id_as_room_id
	{
		let room_create_event_id = incoming_event
			.room_id()
			.as_event_id()
			.map_err(|e| {
				err!(Request(InvalidParam(
					"could not construct `m.room.create` event ID from room ID: {e}"
				)))
			})?;

		let Ok(room_create_event) = fetch_event(room_create_event_id.clone()).await else {
			return Err!(Request(NotFound(
				"failed to find `m.room.create` event {room_create_event_id}"
			)));
		};

		if room_create_event.rejected() {
			return Err!("rejected `m.room.create` event {room_create_event_id}");
		}
	}

	Ok(())
}

/// Check whether the incoming event passes the state-dependent [authorization
/// rules] for the given room version rules.
///
/// The state-dependent rules are all the remaining rules not checked by
/// [`check_state_independent_auth_rules()`].
///
/// This method should be called several times for an event, to perform the
/// [checks on receipt of a PDU].
///
/// The `fetch_state` closure should gather state from a state snapshot. We need
/// to know if the event passes auth against some state not a recursive
/// collection of auth_events fields.
///
/// This assumes that `ruma_signatures::verify_event()` was called previously,
/// as some authorization rules depend on the signatures being valid on the
/// event.
///
/// # Errors
///
/// If the check fails, this returns an `Err(_)` with a description of the check
/// that failed.
///
/// [authorization rules]: https://spec.matrix.org/latest/server-server-api/#authorization-rules
/// [checks on receipt of a PDU]: https://spec.matrix.org/latest/server-server-api/#checks-performed-on-receipt-of-a-pdu
#[tracing::instrument(
	name = "dependent",
	level = "debug",
	skip_all,
	fields(
		sender = ?incoming_event.sender(),
	)
)]
pub(super) async fn check_state_dependent_auth_rules<Fetch, Fut, Pdu>(
	rules: &RoomVersionRules,
	incoming_event: &Pdu,
	fetch_state: &Fetch,
) -> Result
where
	Fetch: Fn(StateEventType, StateKey) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	// There are no state-dependent auth rules for create events.
	if *incoming_event.event_type() == TimelineEventType::RoomCreate {
		trace!("allowing `m.room.create` event");
		return Ok(());
	}

	let sender = incoming_event.sender();
	let (room_create_event, sender_membership, current_room_power_levels_event) = join3(
		fetch_state.room_create_event(),
		fetch_state.user_membership(sender),
		fetch_state.room_power_levels_event(),
	)
	.await;

	// Since v1, if the create event content has the field m.federate set to false
	// and the sender domain of the event does not match the sender domain of the
	// create event, reject.
	let room_create_event = room_create_event?;
	let federate = room_create_event.federate()?;
	if !federate
		&& room_create_event.sender().server_name() != incoming_event.sender().server_name()
	{
		return Err!(
			"room is not federated and event's sender domain does not match `m.room.create` \
			 event's sender domain"
		);
	}

	// v1-v5, if type is m.room.aliases:
	if rules.authorization.special_case_room_aliases
		&& *incoming_event.event_type() == TimelineEventType::RoomAliases
	{
		trace!("starting m.room.aliases check");
		// v1-v5, if event has no state_key, reject.
		//
		// v1-v5, if sender's domain doesn't match state_key, reject.
		if incoming_event.state_key() != Some(sender.server_name().as_str()) {
			return Err!(
				"server name of the `state_key` of `m.room.aliases` event does not match the \
				 server name of the sender"
			);
		}

		// Otherwise, allow.
		trace!("`m.room.aliases` event was allowed");
		return Ok(());
	}

	// Since v1, if type is m.room.member:
	if *incoming_event.event_type() == TimelineEventType::RoomMember {
		let room_member_event = RoomMemberEvent::new(incoming_event.clone());
		return check_room_member(
			&room_member_event,
			&rules.authorization,
			&room_create_event,
			fetch_state,
		)
		.boxed()
		.await;
	}

	// Since v1, if the sender's current membership state is not join, reject.
	let sender_membership = sender_membership?;
	if sender_membership != MembershipState::Join {
		return Err!("sender's membership `{sender_membership}` is not `join`");
	}

	let creators = room_create_event.creators(&rules.authorization)?;
	let sender_power_level = current_room_power_levels_event.user_power_level(
		sender,
		creators.clone(),
		&rules.authorization,
	)?;

	// Since v1, if type is m.room.third_party_invite:
	if *incoming_event.event_type() == TimelineEventType::RoomThirdPartyInvite {
		// Since v1, allow if and only if sender's current power level is greater than
		// or equal to the invite level.
		let invite_power_level = current_room_power_levels_event
			.get_as_int_or_default(RoomPowerLevelsIntField::Invite, &rules.authorization)?;

		if sender_power_level < invite_power_level {
			return Err!(
				"sender does not have enough power ({sender_power_level:?}) to send invites \
				 ({invite_power_level}) in this room"
			);
		}

		trace!("`m.room.third_party_invite` event was allowed");
		return Ok(());
	}

	// Since v1, if the event type's required power level is greater than the
	// sender's power level, reject.
	let event_type_power_level = current_room_power_levels_event.event_power_level(
		incoming_event.event_type(),
		incoming_event.state_key(),
		&rules.authorization,
	)?;

	if sender_power_level < event_type_power_level {
		return Err!(
			"sender does not have enough power ({sender_power_level:?}) for `{}` event type \
			 ({event_type_power_level})",
			incoming_event.event_type()
		);
	}

	// Since v1, if the event has a state_key that starts with an @ and does not
	// match the sender, reject.
	if incoming_event
		.state_key()
		.is_some_and(|k| k.starts_with('@'))
		&& incoming_event.state_key() != Some(incoming_event.sender().as_str())
	{
		return Err!("sender cannot send event with `state_key` matching another user's ID");
	}

	// If type is m.room.power_levels
	if *incoming_event.event_type() == TimelineEventType::RoomPowerLevels {
		let room_power_levels_event = RoomPowerLevelsEvent::new(incoming_event.clone());
		return check_room_power_levels(
			&room_power_levels_event,
			current_room_power_levels_event.as_ref(),
			&rules.authorization,
			sender_power_level,
			creators,
		);
	}

	// v1-v2, if type is m.room.redaction:
	if rules.authorization.special_case_room_redaction
		&& *incoming_event.event_type() == TimelineEventType::RoomRedaction
	{
		return check_room_redaction(
			incoming_event,
			current_room_power_levels_event.as_ref(),
			&rules.authorization,
			sender_power_level,
		);
	}

	// Otherwise, allow.
	trace!("allowing event passed all checks");
	Ok(())
}

/// Check whether the given event passes the `m.room.create` authorization
/// rules.
#[tracing::instrument(level = "trace", skip_all)]
fn check_room_create<Pdu>(
	room_create_event: &RoomCreateEvent<Pdu>,
	rules: &AuthorizationRules,
) -> Result
where
	Pdu: Event,
{
	// Since v1, if it has any previous events, reject.
	if room_create_event.prev_events().next().is_some() {
		return Err!("`m.room.create` event cannot have previous events");
	}

	if rules.room_create_event_id_as_room_id {
		let Ok(room_create_event_id) = room_create_event.room_id().as_event_id() else {
			return Err!(Request(InvalidParam(
				"Failed to create `event_id` out of `m.room.create` synthetic `room_id`"
			)));
		};

		if room_create_event_id != room_create_event.event_id() {
			return Err!(Request(InvalidParam(
				"`m.room.create` has mismatching synthetic `room_id` and `event_id`"
			)));
		}
	} else {
		// v1-v11, if the domain of the room_id does not match the domain of the sender,
		// reject.
		let Some(room_id_server_name) = room_create_event.room_id().server_name() else {
			return Err!("Invalid `ServerName` for `room_id` in `m.room.create` event");
		};

		if room_id_server_name != room_create_event.sender().server_name() {
			return Err!(
				"Mismatched `ServerName` for `room_id` in `m.room.create` with `sender`"
			);
		}
	}

	// Since v1, if `content.room_version` is present and is not a recognized
	// version, reject.
	//
	// This check is assumed to be done before calling auth_check because we have an
	// AuthorizationRules, which means that we recognized the version.

	// v1-v10, if content has no creator field, reject.
	if !rules.use_room_create_sender && !room_create_event.has_creator()? {
		return Err!("missing `creator` field in `m.room.create` event");
	}

	// Otherwise, allow.
	trace!("`m.room.create` event was allowed");
	Ok(())
}

/// Check whether the given event passes the `m.room.power_levels` authorization
/// rules.
#[tracing::instrument(level = "trace", skip_all)]
fn check_room_power_levels<Creators, Pdu>(
	room_power_levels_event: &RoomPowerLevelsEvent<Pdu>,
	current_room_power_levels_event: Option<&RoomPowerLevelsEvent<Pdu>>,
	rules: &AuthorizationRules,
	sender_power_level: impl Into<UserPowerLevel>,
	mut room_creators: Creators,
) -> Result
where
	Creators: Iterator<Item = OwnedUserId> + Clone,
	Pdu: Event,
{
	let sender_power_level = sender_power_level.into();

	// Since v10, if any of the properties users_default, events_default,
	// state_default, ban, redact, kick, or invite in content are present and not
	// an integer, reject.
	let new_int_fields = room_power_levels_event.int_fields_map(rules)?;

	// Since v10, if either of the properties events or notifications in content are
	// present and not a dictionary with values that are integers, reject.
	let new_events = room_power_levels_event.events(rules)?;
	let new_notifications = room_power_levels_event.notifications(rules)?;

	// v1-v9, If the users property in content is not an object with keys that are
	// valid user IDs with values that are integers (or a string that is an
	// integer), reject. Since v10, if the users property in content is not an
	// object with keys that are valid user IDs with values that are integers,
	// reject.
	let new_users = room_power_levels_event.users(rules)?;

	// Since `org.matrix.hydra.11`, if the `users` property in `content` contains
	// the `sender` of

	// the `m.room.create` event or any of the user IDs in the create event's
	// `content.additional_creators`, reject.
	if rules.explicitly_privilege_room_creators
		&& new_users.as_ref().is_some_and(|new_users| {
			room_creators.any(|creator| power_levels::contains_key(new_users, &creator))
		}) {
		return Err!(Request(InvalidParam(
			"creator user IDs are not allowed in the `users` field"
		)));
	}

	trace!("validation of power event finished");

	// Since v1, if there is no previous m.room.power_levels event in the room,
	// allow.
	let Some(current_room_power_levels_event) = current_room_power_levels_event else {
		trace!("initial m.room.power_levels event allowed");
		return Ok(());
	};

	// Since v1, for the properties users_default, events_default, state_default,
	// ban, redact, kick, invite check if they were added, changed or removed. For
	// each found alteration:
	for field in RoomPowerLevelsIntField::ALL {
		let current_power_level = current_room_power_levels_event.get_as_int(*field, rules)?;
		let new_power_level = power_levels::get_value(&new_int_fields, field).copied();

		if current_power_level == new_power_level {
			continue;
		}

		// Since v1, if the current value is higher than the sender’s current power
		// level, reject.
		let current_power_level_too_big =
			current_power_level.unwrap_or_else(|| field.default_value()) > sender_power_level;

		// Since v1, if the new value is higher than the sender’s current power level,
		// reject.
		let new_power_level_too_big =
			new_power_level.unwrap_or_else(|| field.default_value()) > sender_power_level;

		if current_power_level_too_big || new_power_level_too_big {
			return Err!(
				"sender does not have enough power to change the power level of `{field}`"
			);
		}
	}

	// Since v1, for each entry being added to, or changed in, the events property:
	// - Since v1, if the new value is higher than the sender's current power level,
	//   reject.
	let current_events = current_room_power_levels_event.events(rules)?;
	check_power_level_maps(
		current_events.as_deref(),
		new_events.as_deref(),
		sender_power_level,
		|_, current_power_level| {
			// Since v1, for each entry being changed in, or removed from, the events
			// property:
			// - Since v1, if the current value is higher than the sender's current power
			//   level, reject.
			current_power_level > sender_power_level
		},
		|ev_type| {
			err!(
				"sender does not have enough power to change the `{ev_type}` event type power \
				 level"
			)
		},
	)?;

	// Since v6, for each entry being added to, or changed in, the notifications
	// property:
	// - Since v6, if the new value is higher than the sender's current power level,
	//   reject.
	if rules.limit_notifications_power_levels {
		let current_notifications = current_room_power_levels_event.notifications(rules)?;
		check_power_level_maps(
			current_notifications.as_deref(),
			new_notifications.as_deref(),
			sender_power_level,
			|_, current_power_level| {
				// Since v6, for each entry being changed in, or removed from, the notifications
				// property:
				// - Since v6, if the current value is higher than the sender's current power
				//   level, reject.
				current_power_level > sender_power_level
			},
			|key| {
				err!(
					"sender does not have enough power to change the `{key}` notification power \
					 level"
				)
			},
		)?;
	}

	// Since v1, for each entry being added to, or changed in, the users property:
	// - Since v1, if the new value is greater than the sender’s current power
	//   level, reject.
	let current_users = current_room_power_levels_event.users(rules)?;
	check_power_level_maps(
		current_users.as_deref(),
		new_users.as_deref(),
		sender_power_level,
		|user_id, current_power_level| {
			// Since v1, for each entry being changed in, or removed from, the users
			// property, other than the sender’s own entry:
			// - Since v1, if the current value is greater than or equal to the sender’s
			//   current power level, reject.
			user_id != room_power_levels_event.sender()
				&& current_power_level >= sender_power_level
		},
		|user_id| err!("sender does not have enough power to change `{user_id}`'s  power level"),
	)?;

	// Otherwise, allow.
	trace!("m.room.power_levels event allowed");
	Ok(())
}

/// Check the power levels changes between the current and the new maps.
///
/// # Arguments
///
/// * `current`: the map with the current power levels.
/// * `new`: the map with the new power levels.
/// * `sender_power_level`: the power level of the sender of the new map.
/// * `reject_current_power_level_change_fn`: the function to check if a power
///   level change or removal must be rejected given its current value.
///
///   The arguments to the method are the key of the power level and the current
///   value of the power   level. It must return `true` if the change or removal
///   is rejected.
///
///   Note that another check is done after this one to check if the change is
///   allowed given the new   value of the power level.
/// * `error_fn`: the function to generate an error when the change for the
///   given key is not allowed.
fn check_power_level_maps<'a, K>(
	current: Option<&'a [(K, Int)]>,
	new: Option<&'a [(K, Int)]>,
	sender_power_level: UserPowerLevel,
	reject_current_power_level_change_fn: impl FnOnce(&K, Int) -> bool + Copy,
	error_fn: impl FnOnce(&K) -> Error,
) -> Result
where
	K: Ord,
{
	let keys_to_check = current
		.iter()
		.flat_map(|m| m.iter().map(|(k, _)| k))
		.chain(new.iter().flat_map(|m| m.iter().map(|(k, _)| k)));

	for key in keys_to_check {
		let current_power_level = current.and_then(|m| power_levels::get_value(m, key));
		let new_power_level = new.and_then(|m| power_levels::get_value(m, key));

		if current_power_level == new_power_level {
			continue;
		}

		// For each entry being changed in, or removed from, the property.
		let current_power_level_change_rejected = current_power_level
			.is_some_and(|power_level| reject_current_power_level_change_fn(key, *power_level));

		// For each entry being added to, or changed in, the property:
		// - If the new value is higher than the sender's current power level, reject.
		let new_power_level_too_big =
			new_power_level.is_some_and(|&new_power_level| new_power_level > sender_power_level);

		if current_power_level_change_rejected || new_power_level_too_big {
			return Err(error_fn(key));
		}
	}

	Ok(())
}

/// Check whether the given event passes the `m.room.redaction` authorization
/// rules.
fn check_room_redaction<Pdu>(
	room_redaction_event: &Pdu,
	current_room_power_levels_event: Option<&RoomPowerLevelsEvent<Pdu>>,
	rules: &AuthorizationRules,
	sender_level: UserPowerLevel,
) -> Result
where
	Pdu: Event,
{
	let redact_level = current_room_power_levels_event
		.cloned()
		.get_as_int_or_default(RoomPowerLevelsIntField::Redact, rules)?;

	// v1-v2, if the sender’s power level is greater than or equal to the redact
	// level, allow.
	if sender_level >= redact_level {
		trace!("`m.room.redaction` event allowed via power levels");
		return Ok(());
	}

	// v1-v2, if the domain of the event_id of the event being redacted is the same
	// as the domain of the event_id of the m.room.redaction, allow.
	if room_redaction_event.event_id().server_name()
		== room_redaction_event
			.redacts()
			.as_ref()
			.and_then(|&id| id.server_name())
	{
		trace!("`m.room.redaction` event allowed via room version 1 rules");
		return Ok(());
	}

	// Otherwise, reject.
	Err!("`m.room.redaction` event did not pass any of the allow rules")
}
