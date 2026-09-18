use std::collections::{BTreeMap, BTreeSet};

use futures::StreamExt;
use itertools::Itertools;
use ruma::{
	OwnedUserId, RoomId, UserId,
	api::client::{
		filter::FilterDefinition,
		sync::sync_events::v3::{JoinedRoom, Rooms, State, UserUpdate},
	},
	events::{AnySyncStateEvent, StateEventType, room::member::MembershipState},
	profile::ProfileFieldName,
	serde::Raw,
};
use serde::Deserialize;
use serde_json::Value;
use tuwunel_core::{
	Result,
	utils::{IterStream, ReadyExt, result::NotFound, stream::BroadbandExt},
	warn,
};
use tuwunel_service::{Services, profile::ProfileChange};

/// The profile fields one user changed or holds.
///
/// A set rather than a sequence because the two sources overlap: a user can
/// reach the same field through the change log and through the membership this
/// response names, and the field is read back once either way.
pub(super) type Fields = BTreeSet<ProfileFieldName>;

/// Every field the syncing user is entitled to read back, by user.
///
/// The change log is keyed per field, so one user appearing under several rooms
/// or several counts folds into one entry here and is read back once.
pub(super) type Changes = BTreeMap<OwnedUserId, Fields>;

/// One field paired with whatever reading it back produced.
///
/// The error rides per field rather than propagating, so one unreadable row
/// costs that field alone and not the user's entry.
pub(super) type FieldValue = (ProfileFieldName, Result<Option<Value>>);

/// The values one user's entry carries, by field name.
///
/// This is the wire shape MSC4429 gives `profile_updates`, a flat map where a
/// `null` marks a field the profile no longer holds.
type Updates = BTreeMap<ProfileFieldName, Value>;

/// The MSC4429 `users` block of a legacy sync response.
///
/// Empty whenever the client asked for no profile fields, which is the default
/// and what every client that has not opted in sends.
type Users = BTreeMap<OwnedUserId, UserUpdate>;

/// Just the membership of a member event's content.
///
/// The full content type carries fields this pass never reads, and a membership
/// it does not know deserializes into the custom arm rather than failing, where
/// it simply does not match below.
#[derive(Deserialize)]
struct MemberContent {
	membership: MembershipState,
}

/// Collects the MSC4429 profile updates for a legacy sync response.
///
/// The client names the fields it wants in its sync filter, and an empty list,
/// which is the default, asks for nothing at all. Two sources feed the block:
/// the members whose profile changed since the client's token, and the members
/// whose membership this response carries, whose current values ride along so
/// that the client can render them without a profile request per user.
#[tracing::instrument(name = "profiles", level = "trace", skip_all)]
pub(super) async fn collect(
	services: &Services,
	sender_user: &UserId,
	since: u64,
	next_batch: u64,
	filter: &FilterDefinition,
	rooms: &Rooms,
) -> Users {
	let requested = filter.profile_fields.ids.as_slice();

	if requested.is_empty() {
		return Users::new();
	}

	let changes = changed(services, sender_user, since, next_batch, requested).await;
	let changes = witnessed(services, sender_user, since, rooms, requested, changes).await;

	changes
		.into_iter()
		.stream()
		.broad_then(|(user_id, fields)| collect_user(services, user_id, fields))
		.ready_filter(|(_, update)| carries_a_field(update))
		.collect()
		.await
}

/// The fields changed in `(since, next_batch]` that this client may see.
///
/// The log is read under the syncing user's own prefix and under each room they
/// have joined, because every write is recorded under both. Their own changes
/// are a MUST so that their other devices learn of them, and the rooms are the
/// whole joined set rather than the rooms this response carries: a member's new
/// status matters to a client whose room had no events.
async fn changed(
	services: &Services,
	sender_user: &UserId,
	since: u64,
	next_batch: u64,
	requested: &[ProfileFieldName],
) -> Changes {
	let changes = services
		.profile
		.profile_changed(sender_user, since, Some(next_batch))
		.ready_filter(|(_, field)| was_requested(requested, field))
		.ready_fold(Changes::new(), fold_change)
		.await;

	// A cursor stream resolves in its first poll, so a buffered fan-out buys nothing.
	services
		.state_cache
		.rooms_joined(sender_user)
		.map(ToOwned::to_owned)
		.fold(changes, async |changes, room_id| {
			room_changed(services, &room_id, since, next_batch, requested, changes).await
		})
		.await
}

/// Folds the changes one room's members made into the running set.
///
/// The accumulator is threaded rather than merged afterwards, because each
/// room's scan resolves inside its first poll and would only leave a map to
/// reduce.
async fn room_changed(
	services: &Services,
	room_id: &RoomId,
	since: u64,
	next_batch: u64,
	requested: &[ProfileFieldName],
	changes: Changes,
) -> Changes {
	services
		.profile
		.room_profile_changed(room_id, since, Some(next_batch))
		.ready_filter(|(_, field)| was_requested(requested, field))
		.ready_fold(changes, fold_change)
		.await
}

/// Records that one member changed one field.
///
/// Both halves are owned here, at the first point the borrowed change-log item
/// would otherwise be retained.
pub(super) fn fold_change(mut changes: Changes, (user_id, field): ProfileChange<'_>) -> Changes {
	changes
		.entry(user_id.to_owned())
		.or_default()
		.insert(field.into());

	changes
}

/// The current values of the members this response names.
///
/// MSC4429 requires a request with no token to send current values rather than
/// only later changes, and licenses a reduced set when the client lazy-loads
/// members. Reading the membership back out of the response answers both: a
/// lazy-loading client is sent the members it is about to render, and a
/// full-state client everyone it was sent. The syncing user is added on that
/// first request, to name them when they share no room with anybody.
async fn witnessed(
	services: &Services,
	sender_user: &UserId,
	since: u64,
	rooms: &Rooms,
	requested: &[ProfileFieldName],
	changes: Changes,
) -> Changes {
	let own = since.eq(&0).then_some(sender_user.to_owned());

	// A member of several rooms would otherwise be read back once per room.
	let members = rooms
		.join
		.values()
		.flat_map(joined_room_members)
		.chain(own)
		.sorted_unstable()
		.dedup();

	members
		.stream()
		.broad_then(async |user_id| {
			let fields = held(services, &user_id, requested).await;

			(user_id, fields)
		})
		.ready_fold(changes, fold_held)
		.await
}

/// The requested fields this user's profile actually holds.
///
/// Only stored fields are collected, so a field this user never set stays out
/// of the block rather than arriving as the `null` that means a removal.
async fn held(services: &Services, user_id: &UserId, requested: &[ProfileFieldName]) -> Fields {
	services
		.profile
		.profile_field_names(user_id)
		.ready_filter(|name| was_requested(requested, name.as_str()))
		.collect()
		.await
}

fn fold_held(mut changes: Changes, (user_id, fields): (OwnedUserId, Fields)) -> Changes {
	if !fields.is_empty() {
		changes.entry(user_id).or_default().extend(fields);
	}

	changes
}

/// The members one joined room's payload names.
///
/// Both sections are read, because a legacy `state` request omits an event the
/// timeline already carries: a member who joined inside the timeline window
/// appears there and in no other part of the response.
fn joined_room_members(room: &JoinedRoom) -> impl Iterator<Item = OwnedUserId> {
	let state = state_events(&room.state)
		.iter()
		.filter_map(present_member);

	let timeline = room
		.timeline
		.events
		.iter()
		.filter_map(present_member);

	state.chain(timeline)
}

fn state_events(state: &State) -> &[Raw<AnySyncStateEvent>] {
	match state {
		| State::Before(events) | State::After(events) | State::AfterUnstable(events) =>
			events.events.as_slice(),
	}
}

/// The subject of one event, when it is a membership this client still renders.
///
/// A full-state response carries every departure the room ever saw, and MSC4429
/// asks a server not to send profiles for users who share no room, so a
/// membership that has ended names nobody here. The state key is read as an
/// owned id rather than borrowed out of the JSON, because a borrowed `&str`
/// refuses any string the parser had to unescape, and a historical user id may
/// hold the quote or backslash that forces one.
fn present_member<T>(event: &Raw<T>) -> Option<OwnedUserId> {
	event
		.get_field("type")
		.ok()
		.flatten()
		.filter(|event_type: &StateEventType| event_type.eq(&StateEventType::RoomMember))
		.and_then(|_| event.get_field("content").ok().flatten())
		.filter(MemberContent::is_present)
		.and_then(|_| event.get_field("state_key").ok().flatten())
}

impl MemberContent {
	/// Whether the member is in the room, or on their way in.
	///
	/// Invites count because a client renders an invited user's name beside
	/// their pending membership, which is the same reason heroes do.
	fn is_present(&self) -> bool {
		matches!(self.membership, MembershipState::Join | MembershipState::Invite)
	}
}

/// Whether the client's filter asked for the field.
///
/// An empty list never reaches here: MSC4429 defaults it empty, which asks for
/// no updates at all, and the entry point returns early on that.
pub(super) fn was_requested(requested: &[ProfileFieldName], field: &str) -> bool {
	requested
		.iter()
		.any(|name| name.as_str().eq(field))
}

fn carries_a_field(update: &UserUpdate) -> bool {
	update
		.profile_updates
		.as_ref()
		.is_some_and(|updates| !updates.is_empty())
}

async fn collect_user(
	services: &Services,
	user_id: OwnedUserId,
	fields: Fields,
) -> (OwnedUserId, UserUpdate) {
	let update = read_update(services, &user_id, fields).await;

	(user_id, update)
}

/// Reads back what the collected fields hold now.
///
/// The log records that a field changed and never what it changed to, so the
/// current value is the one to send. A field the log names but the profile no
/// longer holds is the removal a client needs to clear its own copy, which the
/// proposal spells as a `null` value.
async fn read_update(services: &Services, user_id: &UserId, fields: Fields) -> UserUpdate {
	let profile_updates = fields
		.into_iter()
		.stream()
		.then(|name| read_field(services, user_id, name))
		.ready_fold(Updates::new(), fold_field)
		.await;

	UserUpdate::new(profile_updates)
}

pub(super) async fn read_field(
	services: &Services,
	user_id: &UserId,
	name: ProfileFieldName,
) -> FieldValue {
	let value = services
		.profile
		.profile_key(user_id, &name)
		.await
		.optional()
		.inspect_err(
			|error| warn!(%user_id, %name, %error, "Failed to read a changed profile field"),
		);

	(name, value)
}

fn fold_field(mut updates: Updates, (name, value): FieldValue) -> Updates {
	// Only an absent field is a removal: a row that fails to read is this
	// server's problem, not a signal to wipe the client's copy.
	if let Ok(value) = value {
		updates.insert(name, value.unwrap_or(Value::Null));
	}

	updates
}

#[cfg(test)]
mod tests {
	use ruma::api::client::sync::sync_events::v3::{StateEvents, Timeline};
	use serde_json::{Value, json};
	use tuwunel_core::Err;

	use super::{
		JoinedRoom, OwnedUserId, ProfileFieldName, Raw, State, Updates, fold_field,
		joined_room_members, was_requested,
	};

	fn field(name: &str) -> ProfileFieldName { name.into() }

	fn members(room: &JoinedRoom) -> Vec<OwnedUserId> { joined_room_members(room).collect() }

	#[test]
	fn only_the_filtered_fields_are_carried() {
		let requested = [field("m.status"), field("displayname")];

		assert!(was_requested(&requested, "m.status"));
		assert!(was_requested(&requested, "displayname"));
		assert!(!was_requested(&requested, "avatar_url"));
		assert!(!was_requested(&[], "m.status"));
	}

	#[test]
	fn an_absent_field_reads_as_a_removal() {
		let updates = [
			(field("m.status"), Ok(Some(json!({"emoji": "🏊"})))),
			(field("displayname"), Ok(None)),
			(field("avatar_url"), Err!("unreadable")),
		]
		.into_iter()
		.fold(Updates::new(), fold_field);

		assert_eq!(updates.get(&field("m.status")), Some(&json!({"emoji": "🏊"})));
		assert_eq!(updates.get(&field("displayname")), Some(&Value::Null));
		assert_eq!(updates.get(&field("avatar_url")), None);
	}

	/// One member event to build: its type, whom it names, and their membership.
	type MemberRow<'a> = (&'a str, &'a str, &'a str);

	fn events<T>(rows: &[MemberRow<'_>]) -> Vec<Raw<T>> {
		rows.iter()
			.map(|(event_type, state_key, membership)| {
				json!({
					"type": event_type,
					"state_key": state_key,
					"content": { "membership": membership },
				})
			})
			.map(|event| Raw::new(&event).expect("event serializes"))
			.map(|event| event.cast_ref_unchecked::<T>().clone())
			.collect()
	}

	#[test]
	fn only_present_members_are_witnessed() {
		let room = JoinedRoom {
			state: State::Before(StateEvents {
				events: events(&[
					("m.room.member", "@alice:example.com", "join"),
					("m.room.topic", "", "join"),
					("m.room.member", "@gone:example.com", "leave"),
					("m.room.member", "@banned:example.com", "ban"),
					("m.room.member", "@asked:example.com", "invite"),
				]),
			}),
			..Default::default()
		};

		assert_eq!(members(&room), ["@alice:example.com", "@asked:example.com"]);
	}

	#[test]
	fn a_state_key_needing_unescaping_is_witnessed() {
		// A borrowed &str refuses a string the parser had to unescape, and the
		// backslash a historical user id may carry forces exactly that.
		let room = JoinedRoom {
			state: State::Before(StateEvents {
				events: events(&[("m.room.member", r"@od\d:example.com", "join")]),
			}),
			..Default::default()
		};

		assert_eq!(members(&room), [r"@od\d:example.com"]);
	}

	#[test]
	fn a_member_joining_inside_the_timeline_is_witnessed() {
		// A legacy `state` request omits an event the timeline already carries.
		let room = JoinedRoom {
			timeline: Timeline {
				events: events(&[("m.room.member", "@late:example.com", "join")]),
				..Default::default()
			},
			..Default::default()
		};

		assert_eq!(members(&room), ["@late:example.com"]);
	}
}
