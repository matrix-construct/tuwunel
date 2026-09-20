use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

use futures::{Stream, StreamExt, TryStreamExt, future::ready, stream::once};
use itertools::{Either, Itertools};
use ruma::{
	OwnedRoomId, OwnedUserId, RoomId, UserId,
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
	utils::{
		BoolExt, IterStream, TryReadyExt,
		result::NotFound,
		stream::{BroadbandExt, TryBroadbandExt},
	},
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

/// Candidate subjects with delta fields or a complete requested base.
///
/// Discovered deltas are restricted to the request selection and are subsumed
/// by a base when the response also witnesses that subject.
#[derive(Default)]
struct Candidates(BTreeMap<OwnedUserId, Candidate>);

type Delta = (OwnedUserId, ProfileFieldName);

enum Input {
	Collected(Candidates),
	Base(OwnedUserId),
	Delta(Delta),
}

/// Delta fields, with an empty set selecting the shared requested base.
///
/// A discovered delta always starts with one field, so the empty marker cannot
/// collide with a delta and needs no additional per-subject discriminant.
struct Candidate(Fields);

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

	let requested: Vec<_> = changes
		.0
		.values()
		.any(is_base)
		.then(|| {
			requested
				.iter()
				.cloned()
				.sorted_unstable()
				.dedup()
				.collect()
		})
		.unwrap_or_default();

	changes
		.0
		.into_iter()
		.stream()
		.broad_then(|(user_id, fields)| {
			collect_user(services, sender_user, user_id, fields, &requested)
		})
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
) -> Result<Candidates> {
	let rooms: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_joined_checked(sender_user)
		.map_ok(ToOwned::to_owned)
		.try_collect()
		.await?;

	let peers = room_changes(services, &rooms, since, next_batch, requested);

	services
		.profile
		.try_profile_changed(sender_user, since, Some(next_batch))
		.ready_try_filter(move |(_, field)| was_requested(requested, field))
		.map_ok(own_change)
		.chain(peers)
		.try_collect()
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
fn room_changes<'a>(
	services: &'a Services,
	rooms: &'a [OwnedRoomId],
	since: u64,
	next_batch: u64,
	requested: &'a [ProfileFieldName],
) -> impl Stream<Item = Result<Input>> + Send + 'a {
	rooms
		.iter()
		.map(move |room_id: &OwnedRoomId| {
			room_changed(services, room_id, since, next_batch, requested)
		})
		.stream()
		.flatten()
}

#[tracing::instrument(level = "trace", skip_all)]
fn room_changed<'a>(
	services: &'a Services,
	room_id: &'a RoomId,
	since: u64,
	next_batch: u64,
	requested: &'a [ProfileFieldName],
) -> impl Stream<Item = Result<Input>> + Send + 'a {
	services
		.profile
		.try_room_profile_changed(room_id, since, Some(next_batch))
		.ready_try_filter(move |(_, field)| was_requested(requested, field))
		.map_ok(own_change)
}

fn own_change((user_id, field): ProfileChange<'_>) -> Input {
	Input::Delta((user_id.to_owned(), field.into()))
}

impl<T> FromIterator<T> for Candidates
where
	Self: Extend<T>,
{
	fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
		let mut candidates = Self::default();
		candidates.extend(iter);

		candidates
	}
}

impl Extend<Input> for Candidates {
	fn extend<T: IntoIterator<Item = Input>>(&mut self, iter: T) {
		for input in iter {
			match input {
				| Input::Collected(candidates) => merge_candidates(self, candidates),
				| Input::Delta(change) => insert_delta(self, change),
				| Input::Base(user_id) => {
					self.0.insert(user_id, Candidate(Fields::new()));
				},
			}
		}
	}
}

fn insert_delta(candidates: &mut Candidates, (user_id, field): Delta) {
	match candidates.0.entry(user_id) {
		| Entry::Vacant(entry) => {
			entry.insert(Candidate([field].into()));
		},
		| Entry::Occupied(entry) =>
			if !is_base(entry.get()) {
				entry.into_mut().0.insert(field);
			},
	}
}

fn merge_candidates(current: &mut Candidates, incoming: Candidates) {
	if current.0.is_empty() {
		current.0 = incoming.0;
	} else {
		for subject in incoming.0 {
			merge_subject(current, subject);
		}
	}
}

fn merge_subject(candidates: &mut Candidates, (user_id, incoming): (OwnedUserId, Candidate)) {
	match candidates.0.entry(user_id) {
		| Entry::Occupied(entry) => merge_fields(entry.into_mut(), incoming),
		| Entry::Vacant(entry) => {
			entry.insert(incoming);
		},
	}
}

fn merge_fields(current: &mut Candidate, incoming: Candidate) {
	match incoming {
		| incoming if is_base(&incoming) => current.0.clear(),
		| incoming if !is_base(current) => current.0.extend(incoming.0),
		| _ => {},
	}
}

fn is_base(candidate: &Candidate) -> bool { candidate.0.is_empty() }

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
	changes: Candidates,
) -> Result<Candidates> {
	let own = initial.then_some(sender_user.to_owned());
	let members = rooms
		.join
		.values()
		.flat_map(joined_room_members)
		.chain(own)
		.map(Input::Base);

	let members = once(ready(Ok(Input::Collected(changes)))).chain(members.try_stream());

	if !initial {
		return members.try_collect().await;
	}

	let rooms: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_joined_checked(sender_user)
		.map_ok(ToOwned::to_owned)
		.try_collect()
		.await?;

	let initial = initial_bases(services, &rooms, filter);

	members.chain(initial).try_collect().await
}

#[tracing::instrument(level = "trace", skip_all)]
fn initial_bases<'a>(
	services: &'a Services,
	rooms: &'a [OwnedRoomId],
	filter: &'a FilterDefinition,
) -> impl Stream<Item = Result<Input>> + Send + 'a {
	rooms
		.iter()
		.try_stream()
		.broad_and_then(move |room_id: &OwnedRoomId| initial_room(services, room_id, filter))
		.ready_try_filter_map(Result::Ok)
		.map_ok(move |room_id: &RoomId| initial_members(services, room_id))
		.try_flatten()
		.map_ok(Input::Base)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn initial_room<'a>(
	services: &Services,
	room_id: &'a RoomId,
	filter: &FilterDefinition,
) -> Result<Option<&'a RoomId>> {
	let lazy = lazy_room(services, room_id, filter)
		.await?
		.is_false()
		.then_some(room_id);

	Ok(lazy)
}

#[tracing::instrument(level = "trace", skip_all)]
fn initial_members<'a>(
	services: &'a Services,
	room_id: &'a RoomId,
) -> impl Stream<Item = Result<OwnedUserId>> + Send + 'a {
	services
		.state_cache
		.room_members_checked(room_id)
		.map_ok(ToOwned::to_owned)
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
	fields: Candidate,
	requested: &[ProfileFieldName],
) -> Result<Option<(OwnedUserId, UserUpdate)>> {
	if !visible(services, sender_user, &user_id).await? {
		return Ok(None);
	}

	let fields = selected_fields(fields, requested);
	let update = read_update(services, &user_id, fields).await?;

	Ok(Some((user_id, update)))
}

fn selected_fields(
	fields: Candidate,
	requested: &[ProfileFieldName],
) -> impl Iterator<Item = ProfileFieldName> + '_ {
	if is_base(&fields) {
		Either::Left(requested.iter().cloned())
	} else {
		Either::Right(fields.0.into_iter())
	}
}

#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn visible(
	services: &Services,
	sender_user: &UserId,
	user_id: &UserId,
) -> Result<bool> {
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
	fields: impl Iterator<Item = ProfileFieldName> + Send,
) -> Result<UserUpdate> {
	let profile_updates: Updates = fields
		.stream()
		.then(|name| read_field(services, user_id, name))
		.map(field_update)
		.try_collect()
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

fn field_update((name, value): FieldValue) -> Result<(ProfileFieldName, Value)> {
	value.map(|value| (name, value.unwrap_or(Value::Null)))
}

#[cfg(test)]
mod tests {
	use std::iter::once;

	use ruma::{
		api::client::sync::sync_events::v3::{StateEvents, Timeline},
		user_id,
	};
	use serde_json::{Value, json};
	use tuwunel_core::{Err, Result};

	use super::{
		Candidates, Input, JoinedRoom, OwnedUserId, ProfileChange, ProfileFieldName, Raw, State,
		Updates, field_update, is_base, joined_room_members, own_change, selected_fields,
		was_requested,
	};

	fn field(name: &str) -> ProfileFieldName { name.into() }

	fn members(room: &JoinedRoom) -> Vec<OwnedUserId> { joined_room_members(room).collect() }

	fn with_delta(mut candidates: Candidates, change: ProfileChange<'_>) -> Candidates {
		candidates.extend([own_change(change)]);

		candidates
	}

	fn with_base(mut candidates: Candidates, user_id: OwnedUserId) -> Candidates {
		candidates.extend([Input::Base(user_id)]);

		candidates
	}

	#[test]
	fn candidate_deltas_union_fields_without_duplicates() {
		let user = user_id!("@alice:example.com");
		let changes = [(user, "org.z"), (user, "org.a"), (user, "org.z")]
			.into_iter()
			.map(own_change)
			.collect::<Candidates>();

		let fields = changes
			.0
			.into_values()
			.next()
			.expect("one subject");

		let selected = selected_fields(fields, &[]).collect::<Vec<_>>();

		assert_eq!(selected, [field("org.a"), field("org.z")]);
	}

	#[test]
	fn vacant_delta_and_existing_base_stay_distinct() {
		let user = user_id!("@alice:example.com");
		let vacant = once((user, "org.z"))
			.map(own_change)
			.collect::<Candidates>();

		let existing = once(Input::Base(user.to_owned())).collect::<Candidates>();

		let existing = with_delta(existing, (user, "org.z"));

		assert!(!is_base(vacant.0.values().next().expect("one delta")));
		assert!(is_base(existing.0.values().next().expect("one base")));
	}

	#[test]
	fn full_bases_subsume_deltas_in_either_order() {
		let user = user_id!("@alice:example.com");
		let requested = [field("org.a"), field("org.z")];

		let delta_first = once((user, "org.z"))
			.map(own_change)
			.collect::<Candidates>();

		let delta_first = with_base(delta_first, user.to_owned());
		let base_first = once(Input::Base(user.to_owned())).collect::<Candidates>();

		let base_first = with_delta(base_first, (user, "org.z"));

		for changes in [delta_first, base_first] {
			let fields = changes
				.0
				.into_values()
				.next()
				.expect("one subject");

			assert!(is_base(&fields), "a base keeps no per-subject field set");

			let selected = selected_fields(fields, &requested).collect::<Vec<_>>();

			assert_eq!(selected, [field("org.a"), field("org.z")]);
		}
	}

	#[test]
	fn repeated_base_subjects_share_one_selection() {
		let alice = user_id!("@alice:example.com");
		let bob = user_id!("@bob:example.com");
		let changes = [bob, alice, bob, alice]
			.into_iter()
			.map(ToOwned::to_owned)
			.map(Input::Base)
			.collect::<Candidates>();

		assert_eq!(changes.0.len(), 2);
		assert!(changes.0.values().all(is_base));
		assert_eq!(changes.0.into_keys().collect::<Vec<_>>(), [alice, bob]);
	}

	#[test]
	fn room_candidate_merges_preserve_union_and_base() {
		let alice = user_id!("@alice:example.com");
		let bob = user_id!("@bob:example.com");

		for order in [[0, 1, 2], [2, 0, 1], [1, 2, 0]] {
			let rooms = order.map(|index| match index {
				| 0 => [(alice, "org.a"), (bob, "org.a")]
					.into_iter()
					.map(own_change)
					.collect::<Candidates>(),
				| 1 => [(alice, "org.z"), (bob, "org.z")]
					.into_iter()
					.map(own_change)
					.collect(),
				| _ => once(Input::Base(alice.to_owned())).collect(),
			});

			let changes: Candidates = rooms.into_iter().map(Input::Collected).collect();

			assert_eq!(changes.0.len(), 2);
			assert!(is_base(changes.0.get(alice).expect("alice base")));

			let bob = changes.0.into_values().last().expect("bob delta");
			let fields = selected_fields(bob, &[]).collect::<Vec<_>>();

			assert_eq!(fields, [field("org.a"), field("org.z")]);
		}
	}

	#[test]
	fn collected_seed_preserves_base_dominance_in_either_order() {
		let user = user_id!("@alice:example.com");

		for seed_first in [true, false] {
			let seed = [(user, "org.a"), (user, "org.z")]
				.into_iter()
				.map(own_change)
				.collect();

			let inputs = match seed_first {
				| true => [Input::Collected(seed), Input::Base(user.to_owned())],
				| false => [Input::Base(user.to_owned()), Input::Collected(seed)],
			};

			let changes: Candidates = inputs.into_iter().collect();

			assert_eq!(changes.0.len(), 1);
			assert!(is_base(changes.0.get(user).expect("base survives seed")));
		}
	}

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
		.map(field_update)
		.collect::<Result<Updates>>()
		.expect("readable fields");

		assert_eq!(updates.get(&field("m.status")), Some(&json!({"emoji": "🏊"})));
		assert_eq!(updates.get(&field("displayname")), Some(&Value::Null));
		field_update((field("avatar_url"), Err!("unreadable")))
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
