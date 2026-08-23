use std::collections::{BTreeMap, BTreeSet};

use futures::StreamExt;
use itertools::Itertools;
use ruma::{
	OwnedUserId, RoomId, UserId,
	api::client::sync::sync_events::v5::response::Profiles,
	profile::{ProfileFieldName, UserProfileChanges, UserProfileUpdate},
};
use serde_json::Value;
use tuwunel_core::{
	Result,
	utils::{IterStream, ReadyExt, stream::BroadbandExt},
};
use tuwunel_service::{Services, profile::ProfileChange, sync::Connection};

use super::{SyncInfo, Window};

type Fields = BTreeSet<ProfileFieldName>;

/// Every field change the syncing user is entitled to see, by user.
///
/// The log is keyed per field, so one user appearing under several rooms or
/// several counts folds into one entry here and is read back once.
type Changes = BTreeMap<OwnedUserId, Fields>;

/// One changed field paired with whatever reading it back produced.
type Field = (ProfileFieldName, Result<Value>);

/// Collects the MSC4262 profiles extension payload.
///
/// The change log is read twice, once under the syncing user's own prefix and
/// once under each room they know, because the log carries a copy of every
/// write under both. Reading a field's current value is deferred until the two
/// passes have folded away the duplicates, so a user who changed one field in
/// forty shared rooms costs one read.
#[tracing::instrument(name = "profiles", level = "trace", skip_all)]
pub(super) async fn collect(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
) -> Result<Profiles> {
	let requested = conn.extensions.profiles.fields.as_deref();

	let changes = services
		.profile
		.profile_changed(sender_user, conn.globalsince, Some(conn.next_batch))
		.ready_filter(|change| was_requested(requested, change))
		.ready_fold(Changes::new(), fold_change)
		.await;

	// Every room the connection knows, not only the window: sliding out of the
	// window does not stop a member's profile from mattering to this client.
	let changes = window
		.keys()
		.merge(conn.rooms.keys())
		.dedup()
		.stream()
		.fold(changes, |changes, room_id| {
			fold_room(changes, services, conn, room_id, requested)
		})
		.await;

	let users = changes
		.into_iter()
		.stream()
		.broad_then(|(user_id, fields)| collect_user(services, user_id, fields))
		.collect()
		.await;

	Ok(Profiles { users })
}

/// Folds the changes one room's members made into the running set.
///
/// The room scans share one accumulator rather than each building its own: a
/// cursor stream resolves inside its first poll, so a fan-out here would buy no
/// concurrency and only leave a map per room to merge afterwards.
async fn fold_room(
	changes: Changes,
	services: &Services,
	conn: &Connection,
	room_id: &RoomId,
	requested: Option<&[ProfileFieldName]>,
) -> Changes {
	services
		.profile
		.room_profile_changed(room_id, changes_from(conn, room_id), Some(conn.next_batch))
		.ready_filter(|change| was_requested(requested, change))
		.ready_fold(changes, fold_change)
		.await
}

/// Where a room's slice of the change log starts.
///
/// A room this connection has never delivered starts at the beginning of the
/// log, which is the initial base MSC4262 asks for when a room enters the
/// window. That slice names exactly the members whose profile ever changed and
/// nobody else, so it costs far less than the member list the proposal warns
/// against sending for a room the size of Matrix HQ.
fn changes_from(conn: &Connection, room_id: &RoomId) -> u64 {
	conn.rooms
		.get(room_id)
		.is_some_and(|room| room.roomsince.gt(&0))
		.then_some(conn.globalsince)
		.unwrap_or_default()
}

/// Whether the connection asked for the changed field.
///
/// An absent filter means every field, which is the only case Element X
/// produces: it never names the fields it wants.
fn was_requested(requested: Option<&[ProfileFieldName]>, (_, field): &ProfileChange<'_>) -> bool {
	requested.is_none_or(|fields| fields.iter().any(|name| name.as_str() == *field))
}

fn fold_change(mut changes: Changes, (user_id, field): ProfileChange<'_>) -> Changes {
	changes
		.entry(user_id.to_owned())
		.or_default()
		.insert(field.into());

	changes
}

async fn collect_user(
	services: &Services,
	user_id: OwnedUserId,
	fields: Fields,
) -> (OwnedUserId, UserProfileUpdate) {
	let update = read_update(services, &user_id, fields).await;

	(user_id, update)
}

/// Reads back what the logged fields hold now.
///
/// The log records that a field changed and never what it changed to, so the
/// current value is the one to send: both proposals want only the latest
/// update, and a field the log names but the profile no longer holds is the
/// removal a client needs to clear its own copy.
async fn read_update(services: &Services, user_id: &UserId, fields: Fields) -> UserProfileUpdate {
	let changes = fields
		.into_iter()
		.stream()
		.then(|name| read_field(services, user_id, name))
		.ready_fold(UserProfileChanges::new(), fold_field)
		.await;

	UserProfileUpdate::Updated(changes)
}

async fn read_field(services: &Services, user_id: &UserId, name: ProfileFieldName) -> Field {
	let value = services.profile.profile_key(user_id, &name).await;

	(name, value)
}

fn fold_field(mut changes: UserProfileChanges, (name, value): Field) -> UserProfileChanges {
	// Only an absent field is a removal: a row that fails to decode is this
	// server's problem, not a signal to wipe the client's copy.
	match value {
		| Err(error) if error.is_not_found() => changes.removed.push(name),
		| Err(_) => (),
		| Ok(value) => {
			changes.updated.insert(name, value);
		},
	}

	changes
}
