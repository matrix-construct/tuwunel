use futures::{Stream, StreamExt, TryFutureExt};
use ruma::{EventId, OwnedEventId, RoomId};
use tuwunel_core::{
	PduId, Result, implement,
	matrix::{Event, Pdu},
	trace,
	utils::{
		stream::{ReadyExt, TryIgnore, WidebandExt},
		u64_from_u8,
	},
};
use tuwunel_database::Interfix;

use super::{
	Service,
	typed_relations::{Tag, prefix},
};

/// Cap on the `m.reference` bundle chunk; /relations is the paginated fallback.
const BUNDLE_MAX: usize = 100;

/// MSC2675/MSC3267: the event ids of `parent`'s `m.reference` children, oldest
/// first, from the typed index, capped at `BUNDLE_MAX`. Empty when
/// `parent` is redacted or unreferenced. The ids come from the index value (the
/// child shorteventid) without loading the children, so the chunk is filtered
/// for neither ignored users nor history visibility. The ignored-user posture
/// matches the /relations endpoint, which also does not filter relation
/// children by ignored sender; the history-visibility posture matches the
/// thread and edit bundles and is less strict than /relations, which does
/// filter children by visibility.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub(super) async fn references(&self, parent: &Pdu) -> Vec<OwnedEventId> {
	if parent.is_redacted() {
		return Vec::new();
	}

	let Ok(parent_id) = self
		.services
		.timeline
		.get_pdu_id(parent.event_id())
		.map_ok(PduId::from)
		.await
	else {
		return Vec::new();
	};

	self.referenced_children(parent_id)
		.take(BUNDLE_MAX)
		.collect()
		.await
}

#[implement(Service)]
fn referenced_children(&self, parent_id: PduId) -> impl Stream<Item = OwnedEventId> + Send + '_ {
	let prefix = prefix(parent_id.shortroomid, parent_id.count, Tag::Reference);
	let seek = prefix.clone();

	self.db
		.relatesto_typed
		.raw_stream_from(seek.as_slice())
		.ignore_err()
		.ready_take_while(move |(key, _)| key.starts_with(&prefix))
		.map(|(_, val)| u64_from_u8(val))
		.wide_filter_map(async |short| {
			self.services
				.short
				.get_eventid_from_short(short)
				.await
				.ok()
		})
}

#[implement(Service)]
#[tracing::instrument(skip_all, level = "debug")]
pub fn mark_as_referenced<'a, I>(&self, room_id: &RoomId, event_ids: I)
where
	I: Iterator<Item = &'a EventId>,
{
	for event_id in event_ids {
		let key = (room_id, event_id);

		self.db.referencedevents.put_raw(key, []);
	}
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug", ret)]
pub async fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> bool {
	let key = (room_id, event_id);

	self.db.referencedevents.qry(&key).await.is_ok()
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn mark_event_soft_failed(&self, event_id: &EventId) {
	self.db.softfailedeventids.insert(event_id, []);
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug", ret)]
pub async fn is_event_soft_failed(&self, event_id: &EventId) -> bool {
	self.db
		.softfailedeventids
		.get(event_id)
		.await
		.is_ok()
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn delete_all_referenced_for_room(&self, room_id: &RoomId) -> Result {
	let prefix = (room_id, Interfix);

	self.db
		.referencedevents
		.keys_prefix_raw(&prefix)
		.ignore_err()
		.ready_for_each(|key| {
			trace!(?key, "Removing key");
			self.db.referencedevents.remove(key);
		})
		.await;

	Ok(())
}
