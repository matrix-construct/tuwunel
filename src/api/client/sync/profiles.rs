use std::collections::{BTreeMap, BTreeSet};

use futures::{StreamExt, TryStreamExt, future::ready};
use itertools::Itertools;
use ruma::{
	OwnedUserId, RoomId, UserId,
	api::client::{
		filter::{FilterDefinition, LazyLoadOptions},
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
	utils::{IterStream, TryReadyExt, result::NotFound, stream::BroadbandExt},
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
/// Keeping the name beside the result lets each collector distinguish a
/// confirmed absence from a storage or decoding failure.
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
/// The filter selects fields, with an empty default opting out. Changes and
/// current bases share a user-field set and require current shared membership,
/// except for self. Read failures abort collection before its position can be
/// acknowledged.
#[tracing::instrument(name = "profiles", level = "trace", skip_all)]
pub(super) async fn collect(
	services: &Services,
	sender_user: &UserId,
	since: Option<u64>,
	next_batch: u64,
	filter: &FilterDefinition,
	rooms: &Rooms,
) -> Result<Users> {
	let requested = filter.profile_fields.ids.as_slice();

	if requested.is_empty() {
		return Ok(Users::new());
	}

	let changes =
		changed(services, sender_user, since.unwrap_or(0), next_batch, requested).await?;

	let changes =
		witnessed(services, sender_user, since.is_none(), rooms, filter, changes).await?;

	changes
		.into_iter()
		.stream()
		.broad_then(|(user_id, fields)| collect_user(services, sender_user, user_id, fields))
		.ready_try_filter_map(Result::Ok)
		.ready_try_filter(|(_, update)| carries_a_field(update))
		.try_collect()
		.await
}

/// The fields changed in `(since, next_batch]` that this client may see.
///
/// The log is read under the syncing user's own prefix and under each room they
/// have joined, because every write is recorded under both. Their own changes
/// are a MUST so that their other devices learn of them, and the rooms are the
/// whole joined set rather than the rooms this response carries: a member's new
/// status matters to a client whose room had no events.
#[tracing::instrument(level = "trace", skip_all)]
async fn changed(
	services: &Services,
	sender_user: &UserId,
	since: u64,
	next_batch: u64,
	requested: &[ProfileFieldName],
) -> Result<Changes> {
	let changes = services
		.profile
		.try_profile_changed(sender_user, since, Some(next_batch))
		.ready_try_filter(|(_, field)| was_requested(requested, field))
		.ready_try_fold(Changes::new(), |changes, change| Ok(fold_change(changes, change)))
		.await?;

	// A cursor stream resolves in its first poll, so a buffered fan-out buys nothing.
	services
		.state_cache
		.rooms_joined_checked(sender_user)
		.map_ok(ToOwned::to_owned)
		.try_fold(changes, async |changes, room_id| {
			room_changed(services, &room_id, since, next_batch, requested, changes).await
		})
		.await
}

/// Folds the changes one room's members made into the running set.
///
/// The accumulator is threaded rather than merged afterwards, because each
/// room's scan resolves inside its first poll and would only leave a map to
/// reduce.
#[tracing::instrument(level = "trace", skip_all)]
async fn room_changed(
	services: &Services,
	room_id: &RoomId,
	since: u64,
	next_batch: u64,
	requested: &[ProfileFieldName],
	changes: Changes,
) -> Result<Changes> {
	services
		.profile
		.try_room_profile_changed(room_id, since, Some(next_batch))
		.ready_try_filter(|(_, field)| was_requested(requested, field))
		.ready_try_fold(changes, |changes, change| Ok(fold_change(changes, change)))
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

/// Adds current bases for the initial scope and members this response names.
///
/// Non-lazy initial bases use current joined membership independently of event
/// filtering. Lazy and incremental responses also introduce the subjects their
/// member events name, while self gets an initial base even without rooms.
#[tracing::instrument(level = "trace", skip_all)]
async fn witnessed(
	services: &Services,
	sender_user: &UserId,
	initial: bool,
	rooms: &Rooms,
	filter: &FilterDefinition,
	changes: Changes,
) -> Result<Changes> {
	let requested = filter.profile_fields.ids.as_slice();
	let own = initial.then_some(sender_user.to_owned());

	// A member of several rooms would otherwise be read back once per room.
	let members = rooms
		.join
		.values()
		.flat_map(joined_room_members)
		.chain(own)
		.sorted_unstable()
		.dedup();

	let changes = members.fold(changes, |changes, user_id| base(changes, user_id, requested));

	if !initial {
		return Ok(changes);
	}

	services
		.state_cache
		.rooms_joined_checked(sender_user)
		.map_ok(ToOwned::to_owned)
		.try_fold(changes, async |changes, room_id| {
			initial_room(services, &room_id, filter, changes).await
		})
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn initial_room(
	services: &Services,
	room_id: &RoomId,
	filter: &FilterDefinition,
	changes: Changes,
) -> Result<Changes> {
	if lazy_room(services, room_id, filter).await? {
		return Ok(changes);
	}

	services
		.state_cache
		.room_members_checked(room_id)
		.ready_try_fold(changes, |changes, user_id| {
			Ok(base(changes, user_id.to_owned(), &filter.profile_fields.ids))
		})
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn lazy_room(
	services: &Services,
	room_id: &RoomId,
	filter: &FilterDefinition,
) -> Result<bool> {
	let options = [&filter.room.state.lazy_load_options, &filter.room.timeline.lazy_load_options];

	if options
		.into_iter()
		.all(LazyLoadOptions::is_disabled)
	{
		return Ok(false);
	}

	let encrypted = services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomEncryption, "")
		.await
		.optional()?;

	Ok(encrypted.is_none())
}

fn base(mut changes: Changes, user_id: OwnedUserId, requested: &[ProfileFieldName]) -> Changes {
	changes
		.entry(user_id)
		.or_default()
		.extend(requested.iter().cloned());

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

/// The subject of a membership event that can introduce a profile base.
///
/// Current shared membership is checked before reading any candidate's values.
/// The state key is owned because a historical user ID may need JSON unescaping.
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
	/// Invited subjects remain candidates when another joined room grants
	/// current shared visibility.
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

#[tracing::instrument(level = "trace", skip_all)]
async fn collect_user(
	services: &Services,
	sender_user: &UserId,
	user_id: OwnedUserId,
	fields: Fields,
) -> Result<Option<(OwnedUserId, UserUpdate)>> {
	if !visible(services, sender_user, &user_id).await? {
		return Ok(None);
	}

	let update = read_update(services, &user_id, fields).await?;

	Ok(Some((user_id, update)))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn visible(services: &Services, sender_user: &UserId, user_id: &UserId) -> Result<bool> {
	if sender_user == user_id {
		return Ok(true);
	}

	services
		.state_cache
		.rooms_joined_checked(user_id)
		.map_ok(ToOwned::to_owned)
		.and_then(async |room_id| {
			let joined = services
				.state_cache
				.get_joined_count(&room_id, sender_user)
				.await
				.optional()?;

			Ok(joined.is_some())
		})
		.try_any(ready)
		.await
}

/// Reads back what the collected fields hold now.
///
/// The log records that a field changed and never what it changed to, so the
/// current value is the one to send. A field the log names but the profile no
/// longer holds is the removal a client needs to clear its own copy, which the
/// proposal spells as a `null` value.
#[tracing::instrument(level = "trace", skip_all)]
async fn read_update(
	services: &Services,
	user_id: &UserId,
	fields: Fields,
) -> Result<UserUpdate> {
	let profile_updates = fields
		.into_iter()
		.stream()
		.then(|name| read_field(services, user_id, name))
		.map(Ok)
		.ready_try_fold(Updates::new(), fold_field)
		.await?;

	Ok(UserUpdate::new(profile_updates))
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

fn fold_field(updates: Updates, (name, value): FieldValue) -> Result<Updates> {
	value.map(|value| insert_field(updates, name, value.unwrap_or(Value::Null)))
}

fn insert_field(mut updates: Updates, name: ProfileFieldName, value: Value) -> Updates {
	updates.insert(name, value);

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
		]
		.into_iter()
		.try_fold(Updates::new(), fold_field)
		.expect("readable fields");

		assert_eq!(updates.get(&field("m.status")), Some(&json!({"emoji": "🏊"})));
		assert_eq!(updates.get(&field("displayname")), Some(&Value::Null));
		fold_field(updates, (field("avatar_url"), Err!("unreadable")))
			.expect_err("unreadable fields abort collection");
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
