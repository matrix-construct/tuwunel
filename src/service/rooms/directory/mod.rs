//! Public room directory storage.
//!
//! The service records which rooms are published and the alias used for each publication.
//! Callers can query individual visibility or stream every published room.

use std::sync::Arc;

use futures::Stream;
use ruma::{OwnedRoomAliasId, RoomAliasId, RoomId, api::client::room::Visibility};
use tuwunel_core::{Result, implement, utils::stream::TryIgnore};
use tuwunel_database::{Deserialized, Map};

/// Stores and queries public room directory entries.
///
/// Each entry is keyed by room ID and optionally retains the alias used to publish it. A room
/// without an entry is treated as private.
pub struct Service {
	db: Data,
}

struct Data {
	publicroomids: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				publicroomids: args.db["publicroomids"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Publishes a room in the directory.
///
/// The optional alias is stored with the room entry. Publishing without an alias stores an empty
/// value while retaining public visibility.
#[implement(Service)]
pub fn set_public(&self, room_id: &RoomId, alias: Option<&RoomAliasId>) {
	self.db
		.publicroomids
		.insert(room_id, alias.map_or("", RoomAliasId::as_str));
}

/// Removes a room from the public directory.
///
/// Removing an absent entry is harmless. Subsequent visibility queries report the room as
/// private.
#[implement(Service)]
pub fn set_not_public(&self, room_id: &RoomId) { self.db.publicroomids.remove(room_id); }

/// Returns the alias under which a room was published.
///
/// Rooms published without an alias store an empty value, which cannot deserialize as a room
/// alias and therefore returns an error.
#[implement(Service)]
pub async fn published_alias(&self, room_id: &RoomId) -> Result<OwnedRoomAliasId> {
	self.db
		.publicroomids
		.get(room_id)
		.await
		.deserialized()
}

/// Streams the room IDs currently published in the directory.
///
/// Each borrowed ID is valid only until the stream is polled again and must be copied before
/// retention. Rows with unparsable room IDs are skipped.
#[implement(Service)]
pub fn public_rooms(&self) -> impl Stream<Item = &RoomId> + Send {
	self.db.publicroomids.keys().ignore_err()
}

/// Reports whether a room is currently public.
///
/// Visibility is determined by the presence of the room's directory entry. Missing entries and
/// failed lookups are treated as private.
#[implement(Service)]
pub async fn is_public_room(&self, room_id: &RoomId) -> bool {
	self.visibility(room_id).await == Visibility::Public
}

/// Returns a room's client-facing directory visibility.
///
/// A stored directory entry maps to [`Visibility::Public`]. Missing entries and failed lookups map
/// to [`Visibility::Private`].
#[implement(Service)]
pub async fn visibility(&self, room_id: &RoomId) -> Visibility {
	if self.db.publicroomids.get(room_id).await.is_ok() {
		Visibility::Public
	} else {
		Visibility::Private
	}
}
