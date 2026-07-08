use futures::TryStreamExt;
use ruma::{RoomId, api::Direction, events::TimelineEventType};
use tuwunel_core::{
	Result, implement,
	matrix::{
		Event,
		pdu::{PduCount, PduEvent},
	},
	trace,
	utils::stream::TryReadyExt,
};

use super::{ExtractBody, RawPduId, bias_count};

/// Selectively purges room history strictly before `until` in stream order,
/// returning the number of events removed. State events are always preserved,
/// and locally-sent events are kept unless `delete_local_events`. Forward
/// extremities are never touched, so the room stays live. tuwunel orders by
/// per-room stream position rather than topological depth, so "same-depth
/// events retained" becomes "strictly earlier in stream order".
#[implement(super::Service)]
pub async fn purge_history(
	&self,
	room_id: &RoomId,
	until: PduCount,
	delete_local_events: bool,
) -> Result<usize> {
	let shortroomid = self
		.services
		.short
		.get_shortroomid(room_id)
		.await?;

	let start = self
		.count_to_id(room_id, PduCount::min(), Direction::Forward)
		.await?;

	let prefix = start.shortroomid();

	self.db
		.pduid_pdu
		.raw_stream_from(&start)
		.ready_try_take_while(move |kv| {
			let (key, _) = *kv;
			Ok(key.starts_with(&prefix) && RawPduId::from(key).pdu_count() < until)
		})
		.try_fold(0_usize, async |purged, (key, value)| {
			let pdu = serde_json::from_slice::<PduEvent>(value)?;

			if pdu.state_key.is_some()
				|| (!delete_local_events && self.services.globals.user_is_local(&pdu.sender))
			{
				return Ok(purged);
			}

			let raw_id = RawPduId::from(key);
			let count = raw_id.pdu_count();
			let event_id = pdu.event_id.clone();
			let ts: u64 = pdu.origin_server_ts.into();

			self.db.pduid_pdu.remove(key);
			self.db.eventid_pduid.remove(&event_id);
			self.db.eventid_outlierpdu.remove(&event_id);
			self.db
				.roomid_tscount_pducount
				.del((room_id, ts, bias_count(raw_id.count())));

			if pdu.kind == TimelineEventType::RoomMessage
				&& let Ok(ExtractBody { body: Some(body) }) = pdu.get_content()
			{
				self.services
					.search
					.deindex_pdu(shortroomid, &raw_id, &body);
			}

			self.services
				.pdu_metadata
				.purge_event_relations(shortroomid, count, room_id, &event_id)
				.await;

			self.services.retention.purge_original(&event_id);

			trace!(?event_id, ?room_id, "Purged");

			Ok(purged.saturating_add(1))
		})
		.await
}
