use std::time::SystemTime;

use ruma::{
	RoomId,
	api::{
		client::space::SpaceHierarchyRoomsChunk,
		federation::space::SpaceHierarchyParentSummary as ParentSummary,
	},
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, at, debug, implement, utils::rand::time_from_now_secs};
use tuwunel_database::{Deserialized, Json};

use super::is_summary_serializable;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Provenance {
	#[default]
	Unknown,
	Local,
	Remote,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Cached {
	pub(super) expires: SystemTime,
	pub(super) summary: Option<ParentSummary>,
	#[serde(default)]
	pub(super) provenance: Provenance,
}

/// Remove the entry for `room_id` from the cache.
#[implement(super::Service)]
#[inline]
#[tracing::instrument(name = "evict", level = "debug", skip_all, fields(room_id))]
pub fn cache_evict(&self, room_id: &RoomId) { self.db.roomid_spacehierarchy.remove(room_id); }

#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip(self, summary),
	fields(summary = summary.is_some())
)]
pub(super) fn cache_put(
	&self,
	room_id: &RoomId,
	summary: Option<&ParentSummary>,
	provenance: Provenance,
) {
	debug!(?room_id, "cache put");
	self.db.roomid_spacehierarchy.raw_put(
		room_id,
		Json(Cached {
			expires: self.generate_ttl(),
			summary: summary.cloned().filter(is_summary_serializable),
			provenance,
		}),
	);
}

#[implement(super::Service)]
#[tracing::instrument(
	level = "trace",
	skip(self),
	err(level = "debug"),
	ret(level = "trace")
)]
pub(super) async fn cache_get(&self, room_id: &RoomId) -> Result<Cached> {
	self.db
		.roomid_spacehierarchy
		.get(room_id)
		.await
		.deserialized::<Json<Cached>>()
		.map(at!(0))
}

/// Here because cannot implement `From` across ruma-federation-api and
/// ruma-client-api types
impl From<Cached> for Option<SpaceHierarchyRoomsChunk> {
	#[inline]
	fn from(value: Cached) -> Self {
		value
			.summary
			.map(|ParentSummary { children_state, summary }: ParentSummary| {
				SpaceHierarchyRoomsChunk { children_state, summary }
			})
	}
}

#[implement(super::Service)]
#[inline]
fn generate_ttl(&self) -> SystemTime {
	time_from_now_secs(
		self.services.config.spacehierarchy_cache_ttl_min
			..self.services.config.spacehierarchy_cache_ttl_max,
	)
}
