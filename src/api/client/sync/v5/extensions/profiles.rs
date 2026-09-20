use futures::{StreamExt, TryStreamExt};
use itertools::Itertools;
use ruma::{
	OwnedUserId, RoomId, UserId,
	api::client::sync::sync_events::v5::response::{Profiles, Room as ResponseRoom},
	events::{StateEventType, room::member::MembershipState},
	profile::{ProfileFieldName, UserProfileChanges, UserProfileUpdate},
	serde::Raw,
};
use serde_json::Value;
use tuwunel_core::{
	Result,
	utils::{
		BoolExt, IterStream,
		stream::{BroadbandExt, TryReadyExt},
	},
};
use tuwunel_service::{Services, sync::Connection};

use super::{
	super::{range::Results, rooms::merged_room_details},
	SyncInfo, Window, selector,
};
use crate::client::sync::profiles::{Changes, Fields, fold_change, read_field, visible};

/// Collects the MSC4262 profiles extension payload.
///
/// Current selected-room bases and bounded discovery share one user-field set.
/// Visibility and read errors resolve before the response position is acknowledged.
#[tracing::instrument(name = "profiles", level = "trace", skip_all)]
pub(super) async fn collect(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
	ranges: &Results,
) -> Result<Profiles> {
	let requested = conn.extensions.profiles.fields.as_deref();
	if requested.is_some_and(<[_]>::is_empty) {
		return Ok(Profiles::default());
	}

	let bases = room_bases(services, conn, window, ranges).await?;
	let changes = bases
		.chain(
			conn.own_profile_owed()
				.then_some(sender_user.to_owned()),
		)
		.sorted_unstable()
		.dedup()
		.stream()
		.broad_then(|user_id| base(services, sender_user, user_id, requested))
		.ready_try_filter_map(Result::Ok)
		.try_collect()
		.await?;

	let changes = services
		.profile
		.try_profile_changed(sender_user, conn.globalsince, Some(conn.next_batch))
		.ready_try_filter(|(_, field)| was_requested(requested, field))
		.ready_try_fold(changes, |changes, change| Ok(fold_change(changes, change)))
		.await?;

	let changes = window
		.keys()
		.merge(conn.rooms.keys())
		.dedup()
		.filter(|_| conn.globalsince != 0)
		.try_stream()
		.try_fold(changes, async |changes, room_id| {
			fold_room(changes, services, conn, room_id, requested).await
		})
		.await?;

	let users = changes
		.into_iter()
		.stream()
		.broad_then(|(user_id, fields)| collect_user(services, sender_user, user_id, fields))
		.ready_try_filter_map(Result::Ok)
		.try_collect()
		.await?;

	Ok(Profiles { users })
}

#[tracing::instrument(level = "trace", skip_all)]
async fn room_bases(
	services: &Services,
	conn: &Connection,
	window: &Window,
	ranges: &Results,
) -> Result<impl Iterator<Item = OwnedUserId>> {
	let config = &conn.extensions.profiles;

	let subjects = selector(
		conn,
		window,
		config.lists.as_ref().map(|lists| lists.iter()),
		config.rooms.as_ref().map(|rooms| rooms.iter()),
	)
	.filter_map(|room_id| {
		ranges
			.payload(room_id)
			.map(|room| (room_id, room))
	})
	.filter(|(_, room)| room.initial.unwrap_or(false) || conn.own_profile_owed())
	.try_stream()
	.try_fold(Vec::new(), async |bases, (room_id, room)| {
		let bases = extend_subjects(bases, subjects(room));

		let Some(selected) = window.get(room_id) else {
			return Ok(bases);
		};

		let (_, state) = merged_room_details(conn, &selected.lists, room_id);
		let lazy = state
			.iter()
			.any(|(kind, key)| kind == &StateEventType::RoomMember && key == "$LAZY");

		let full = state.iter().any(|(kind, key)| {
			(kind == &StateEventType::RoomMember || kind == &StateEventType::from("*"))
				&& key == "*"
		});

		if (lazy && !full) || room.membership != Some(MembershipState::Join) {
			return Ok(bases);
		}

		services
			.state_cache
			.room_members_checked(room_id)
			.map_ok(ToOwned::to_owned)
			.ready_try_fold(bases, |bases, user_id| Ok(extend_subjects(bases, [user_id])))
			.await
	})
	.await?;

	Ok(subjects.into_iter())
}

fn extend_subjects(
	mut subjects: Vec<OwnedUserId>,
	additional: impl IntoIterator<Item = OwnedUserId>,
) -> Vec<OwnedUserId> {
	subjects.extend(additional);
	subjects
}

fn subjects(room: &ResponseRoom) -> impl Iterator<Item = OwnedUserId> + '_ {
	let senders = room
		.timeline
		.iter()
		.filter_map(|event| event.get_field("sender").ok().flatten());

	let members = room
		.timeline
		.iter()
		.filter_map(member)
		.chain(room.required_state.iter().filter_map(member));

	let heroes = room
		.heroes
		.iter()
		.flatten()
		.map(|hero| hero.user_id.clone());

	senders.chain(members).chain(heroes)
}

fn member<T>(event: &Raw<T>) -> Option<OwnedUserId> {
	event
		.get_field("type")
		.ok()
		.flatten()
		.filter(|kind: &StateEventType| kind == &StateEventType::RoomMember)
		.and_then(|_| event.get_field("state_key").ok().flatten())
}

#[tracing::instrument(level = "trace", skip_all)]
async fn base(
	services: &Services,
	sender_user: &UserId,
	user_id: OwnedUserId,
	requested: Option<&[ProfileFieldName]>,
) -> Result<Option<(OwnedUserId, Fields)>> {
	if !visible(services, sender_user, &user_id).await? {
		return Ok(None);
	}

	let fields: Fields = match requested {
		| Some(fields) => fields.iter().cloned().collect(),
		| None =>
			services
				.profile
				.try_profile_field_names(&user_id)
				.try_collect()
				.await?,
	};

	Ok(fields
		.is_empty()
		.is_false()
		.then_some((user_id, fields)))
}

async fn fold_room(
	changes: Changes,
	services: &Services,
	conn: &Connection,
	room_id: &RoomId,
	requested: Option<&[ProfileFieldName]>,
) -> Result<Changes> {
	services
		.profile
		.try_room_profile_changed(room_id, changes_from(conn, room_id), Some(conn.next_batch))
		.ready_try_filter(|(_, field)| was_requested(requested, field))
		.ready_try_fold(changes, |changes, change| Ok(fold_change(changes, change)))
		.await
}

fn changes_from(conn: &Connection, room_id: &RoomId) -> u64 {
	conn.rooms
		.get(room_id)
		.is_some_and(|room| room.roomsince.gt(&0) && conn.profiles_fields_owed().is_false())
		.then_some(conn.globalsince)
		.unwrap_or_default()
}

fn was_requested(requested: Option<&[ProfileFieldName]>, field: &str) -> bool {
	requested.is_none_or(|fields| fields.iter().any(|name| name.as_str() == field))
}

async fn collect_user(
	services: &Services,
	sender_user: &UserId,
	user_id: OwnedUserId,
	fields: Fields,
) -> Result<Option<(OwnedUserId, UserProfileUpdate)>> {
	if !visible(services, sender_user, &user_id).await? {
		return Ok(None);
	}

	let update = read_update(services, &user_id, fields).await?;

	Ok(Some((user_id, update)))
}

async fn read_update(
	services: &Services,
	user_id: &UserId,
	fields: Fields,
) -> Result<UserProfileUpdate> {
	let changes = fields
		.into_iter()
		.stream()
		.then(|name| read_field(services, user_id, name))
		.map(|(name, value)| value.map(|value| (name, value)))
		.ready_try_fold(UserProfileChanges::new(), |changes, field| {
			Ok(fold_field(changes, field))
		})
		.await?;

	Ok(UserProfileUpdate::Updated(changes))
}

fn fold_field(
	mut changes: UserProfileChanges,
	(name, value): (ProfileFieldName, Option<Value>),
) -> UserProfileChanges {
	match value {
		| None => changes.removed.push(name),
		| Some(value) => {
			changes.updated.insert(name, value);
		},
	}

	changes
}

#[cfg(test)]
mod tests {
	use ruma::room_id;
	use tuwunel_service::sync::{Connection, Room};

	use super::changes_from;

	#[test]
	fn a_widened_field_set_replays_a_known_room() {
		let room_id = room_id!("!known:example.com");
		let mut conn = Connection {
			globalsince: 7,
			rooms: [(room_id.to_owned(), Room { roomsince: 3, ..Default::default() })].into(),
			..Default::default()
		};

		conn.extensions.profiles.enabled = Some(true);

		assert_eq!(changes_from(&conn, room_id), 7);

		conn.profiles_fields_widened = true;

		assert_eq!(changes_from(&conn, room_id), 0);

		conn.own_profile_since = 7;

		assert_eq!(changes_from(&conn, room_id), 7);
	}
}
