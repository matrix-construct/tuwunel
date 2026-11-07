use ruma::{
	EventId, RoomId,
	events::{relation::RelationType, room::encrypted::Relation},
};
use tuwunel_core::{
	Result,
	arrayvec::ArrayVec,
	debug_warn, implement, info,
	result::NotFound,
	utils::stream::{ReadyExt, TryBroadbandExt, TryIgnore, TryReadyExt},
};
use tuwunel_matrix::{Event, Pdu, PduCount, PduId, RawPduId};

use super::{
	ExtractRelatesTo, Service,
	typed_relations::{Derived, KEY_LEN, indexed_key, tag},
};
use crate::{
	integrity::relatesto_typed::{Expected, Row, expected},
	rooms::short::ShortRoomId,
};

type Prefix = ArrayVec<u8, 16>;

/// What the relation rebuild made of one event row.
///
/// A relation whose parent cannot be resolved is skipped and counted, so the
/// rebuild carries on past a bad read or a damaged mapping.
enum Indexed {
	Written,
	Skipped,
	Unparsable,
}

/// Purges one event's metadata during a history purge.
///
/// Relation rows keyed by this event as parent or target are removed. Rows
/// keyed by it as a surviving event's child remain dangling because relation
/// reads discard IDs that no longer resolve. Soft-fail and policy decisions are
/// cleared after the relation indexes.
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

	self.services
		.event_handler
		.clear_policy_signature_state(event_id);

	self.clear_event_soft_failed(event_id);
}

/// Rebuilds `relatesto_typed` from every stored PDU.
///
/// Runs once at startup behind a `global` marker, and on demand from the admin
/// command. It clears the index first, so a partial or stale one is replaced
/// wholesale, then writes the rows the relation index check expects. A cursor
/// failure ends it with an error, leaving the index partial, while a relation
/// whose parent lookup fails, or whose parent mapping is unreadable, is skipped
/// and counted.
#[implement(Service)]
pub async fn rebuild_typed_relations(&self) -> Result {
	self.db.relatesto_typed.clear().await;

	let (written, skipped, unparsable) = expected(&self.services)
		.broad_and_then(async |expected| Ok(self.index_expected(expected).await))
		.ready_try_fold((0_usize, 0_usize, 0_usize), |(written, skipped, unparsable), indexed| {
			Ok(match indexed {
				| Indexed::Written => (written.saturating_add(1), skipped, unparsable),
				| Indexed::Skipped => (written, skipped.saturating_add(1), unparsable),
				| Indexed::Unparsable => (written, skipped, unparsable.saturating_add(1)),
			})
		})
		.await?;

	info!(%written, %skipped, %unparsable, "Rebuilt relatesto_typed relation index");

	Ok(())
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn index_expected(&self, expected: Expected) -> Indexed {
	match expected {
		| Expected::Unparsable(_) => Indexed::Unparsable,
		| Expected::Unresolved { pdu_id, error } => {
			debug_warn!(?pdu_id, %error, "Relation not indexed, its parent lookup failed");

			Indexed::Skipped
		},
		| Expected::UnreadableParent { child, parent } => {
			debug_warn!(%child, %parent, "Relation not indexed, its parent mapping is unreadable");

			Indexed::Skipped
		},
		| Expected::Row(row) => {
			self.index_row(row).await;

			Indexed::Written
		},
	}
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn index_row(&self, Row { key, child }: Row) {
	let child_short = self
		.services
		.short
		.get_or_create_shorteventid(&child)
		.await;

	self.db
		.relatesto_typed
		.aput_raw::<KEY_LEN, _, _>(key.as_slice(), child_short.to_be_bytes());
}

/// The `relatesto_typed` key an event is indexed under, if any.
///
/// Only an `m.replace` or `m.reference` relation is indexed, and only as
/// [`Service::add_typed_relation`] indexes it. Unlike that live path, the
/// parent's mapping is decoded without trusting it, so a damaged one is reported
/// rather than panicking. A lookup failing other than as absent is returned.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn typed_relation_key(&self, pdu_id: PduId, pdu: &Pdu) -> Result<Derived> {
	let Ok(content) = pdu.get_content::<ExtractRelatesTo>() else {
		return Ok(Derived::Unindexed);
	};

	let (rel_type, parent) = match content.relates_to {
		| Relation::Replacement(replacement) => (RelationType::Replacement, replacement.event_id),
		| Relation::Reference(reference) => (RelationType::Reference, reference.event_id),
		| _ => return Ok(Derived::Unindexed),
	};

	let Some(tag) = tag(&rel_type) else {
		return Ok(Derived::Unindexed);
	};

	let Some(parent_id) = self
		.db
		.eventid_pduid
		.get(&parent)
		.await
		.optional()?
		.map(|parent_id| RawPduId::try_from_bytes(&parent_id))
	else {
		return Ok(Derived::Unindexed);
	};

	let Some(parent_id) = parent_id else {
		return Ok(Derived::UnreadableParent(parent));
	};

	let key = indexed_key(pdu_id.shortroomid, parent_id.pdu_count(), tag, pdu, pdu_id.count);

	let derived = key.map_or(Derived::Unindexed, Derived::Key);

	Ok(derived)
}
