use futures::StreamExt;
use ruma::{
	EventId, RoomId,
	events::{relation::RelationType, room::encrypted::Relation},
};
use tuwunel_core::{
	PduId, Result,
	arrayvec::ArrayVec,
	implement,
	matrix::{Event, Pdu, PduCount, RawPduId},
	utils::{
		stream::{ReadyExt, TryIgnore, automatic_width},
		u64_from_u8,
	},
};

use super::{ExtractRelatesTo, Service};
use crate::rooms::short::ShortRoomId;

type Prefix = ArrayVec<u8, 16>;

/// Purges one event's relation-index rows during a history purge. Rows keyed by
/// this event as parent/target are removed; rows keyed by it as a surviving
/// event's child are left dangling (harmless: relation reads discard ids that
/// no longer resolve).
#[implement(Service)]
pub async fn purge_event_relations(
	&self,
	shortroomid: ShortRoomId,
	parent: PduCount,
	room_id: &RoomId,
	event_id: &EventId,
) {
	let target = parent.to_be_bytes();

	self.db
		.tofrom_relation
		.raw_keys_from(target.as_slice())
		.ignore_err()
		.ready_take_while(move |key| key.starts_with(&target))
		.ready_for_each(|key| self.db.tofrom_relation.remove(key))
		.await;

	let mut prefix = Prefix::new();

	prefix.extend(shortroomid.to_be_bytes());
	prefix.extend(parent.to_be_bytes());

	self.db
		.relatesto_typed
		.raw_keys_from(prefix.as_slice())
		.ignore_err()
		.ready_take_while(move |key| key.starts_with(&prefix))
		.ready_for_each(|key| self.db.relatesto_typed.remove(key))
		.await;

	self.db.referencedevents.del((room_id, event_id));

	self.db.softfailedeventids.remove(event_id);
}

/// Rebuild `relatesto_typed` from every stored PDU. Run once at startup behind
/// a `global` marker, and on demand from the admin command. Clears first so a
/// partial or stale index is replaced wholesale.
#[implement(Service)]
pub async fn rebuild_typed_relations(&self) -> Result {
	self.db.relatesto_typed.clear().await;

	let pdus = self.services.db["pduid_pdu"].clone();

	pdus.raw_stream()
		.ignore_err()
		.ready_filter_map(|(key, value)| {
			let raw_pdu_id = RawPduId::from(key);
			let pdu_id = PduId {
				shortroomid: u64_from_u8(&raw_pdu_id.shortroomid()),
				count: raw_pdu_id.pdu_count(),
			};
			let pdu = serde_json::from_slice::<Pdu>(value).ok()?;

			Some((pdu_id, pdu))
		})
		.for_each_concurrent(automatic_width(), async |(pdu_id, pdu)| {
			self.index_pdu_relations(pdu_id, &pdu).await;
		})
		.await;

	Ok(())
}

#[implement(Service)]
async fn index_pdu_relations(&self, pdu_id: PduId, pdu: &Pdu) {
	let Ok(content) = pdu.get_content::<ExtractRelatesTo>() else {
		return;
	};

	let (rel_type, parent) = match content.relates_to {
		| Relation::Replacement(replacement) => (RelationType::Replacement, replacement.event_id),
		| Relation::Reference(reference) => (RelationType::Reference, reference.event_id),
		| _ => return,
	};

	self.add_typed_relation(pdu_id.shortroomid, pdu_id.count, &parent, pdu, rel_type)
		.await;
}
