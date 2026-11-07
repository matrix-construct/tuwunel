use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomId,
	events::relation::RelationType,
};
use tuwunel_core::{
	Result,
	arrayvec::ArrayVec,
	implement,
	result::NotFound,
	utils::{
		stream::{ReadyExt, TryIgnore},
		u64_from_u8,
	},
};
use tuwunel_matrix::{Event, PduCount, PduId, RawPduId};

use super::Service;
use crate::rooms::short::ShortRoomId;

/// `relatesto_typed` key buffer, sized to the writer's fixed length.
///
/// A key that fails the slice conversion cannot be a relation row, the
/// writer emitting no other length.
pub(crate) type Key = ArrayVec<u8, KEY_LEN>;

type Prefix = ArrayVec<u8, PREFIX_LEN>;

/// What deriving an event's `relatesto_typed` row from storage found.
///
/// The derivation reads the parent's mapping without trusting it, so a damaged
/// mapping is an answer of its own rather than a panic.
pub(crate) enum Derived {
	/// The key the event is indexed under.
	Key(Key),

	/// The event is not an indexed relation.
	Unindexed,

	/// The parent's `eventid_pduid` value is not a PDU id.
	UnreadableParent(OwnedEventId),
}

/// `relatesto_typed` rel_type discriminant, occupying one key byte between the
/// parent `RawPduId` and the child's ts. Stable on-disk format; the explicit
/// discriminants are permanent and must stay distinct.
#[derive(Clone, Copy)]
pub(super) enum Tag {
	Replace = 0x01,
	Reference = 0x02,
}

impl From<Tag> for u8 {
	#[inline]
	fn from(tag: Tag) -> Self {
		match tag {
			| Tag::Replace => 0x01,
			| Tag::Reference => 0x02,
		}
	}
}

/// `relatesto_typed` seek prefix: `shortroomid || parent_count || tag`.
pub(super) const PREFIX_LEN: usize = size_of::<u64>() * 2 + size_of::<u8>();

/// `relatesto_typed` key: the prefix followed by `child_ts || child_count`.
pub(super) const KEY_LEN: usize = PREFIX_LEN + size_of::<u64>() * 2;

/// `relatesto_typed` key: byte offset of the child `PduCount` (the key tail).
pub(super) const CHILD_COUNT_OFFSET: usize = KEY_LEN - size_of::<u64>();

/// Maintain the `rel_type`-aware relation index for an `m.replace` or
/// `m.reference` child of `parent`. The row is keyed by the parent so a serve
/// of `parent` seeks its newest edit (or its references) without loading
/// non-matching children. Indexed unconditionally; only the read fold is gated.
#[implement(Service)]
#[tracing::instrument(skip(self, child), level = "debug")]
pub async fn add_typed_relation<E: Event>(
	&self,
	shortroomid: ShortRoomId,
	child_count: PduCount,
	parent: &EventId,
	child: &E,
	rel_type: RelationType,
) {
	let Ok(Some(key)) = self
		.relation_key(shortroomid, child_count, parent, child, &rel_type)
		.await
	else {
		return;
	};

	let child_short = self
		.services
		.short
		.get_or_create_shorteventid(child.event_id())
		.await;

	self.db
		.relatesto_typed
		.aput_raw::<KEY_LEN, _, _>(key.as_slice(), child_short.to_be_bytes());
}

/// The key an `m.replace` or `m.reference` child of `parent` is indexed under.
///
/// Another relation type, a parent not stored, or a backfilled event on either
/// side gives `None`. A parent lookup failing other than as absent is returned.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn relation_key<E: Event>(
	&self,
	shortroomid: ShortRoomId,
	child_count: PduCount,
	parent: &EventId,
	child: &E,
	rel_type: &RelationType,
) -> Result<Option<Key>> {
	let Some(tag) = tag(rel_type) else {
		return Ok(None);
	};

	let key = self
		.services
		.timeline
		.get_pdu_count(parent)
		.await
		.optional()?
		.and_then(|parent_count| indexed_key(shortroomid, parent_count, tag, child, child_count));

	Ok(key)
}

pub(super) fn tag(rel_type: &RelationType) -> Option<Tag> {
	match rel_type {
		| RelationType::Replacement => Some(Tag::Replace),
		| RelationType::Reference => Some(Tag::Reference),
		| _ => None,
	}
}

/// The key a relation is indexed under, once its parent's position is known.
///
/// A backfilled event on either side is not indexed, and gives `None`.
pub(super) fn indexed_key<E: Event>(
	shortroomid: ShortRoomId,
	parent_count: PduCount,
	tag: Tag,
	child: &E,
	child_count: PduCount,
) -> Option<Key> {
	let (PduCount::Normal(_), PduCount::Normal(_)) = (parent_count, child_count) else {
		return None;
	};

	let child_ts = u64::from(child.origin_server_ts().get());

	Some(key(shortroomid, parent_count, tag, child_ts, child_count))
}

pub(super) fn key(
	shortroomid: ShortRoomId,
	parent: PduCount,
	tag: Tag,
	child_ts: u64,
	child: PduCount,
) -> Key {
	let mut key = ArrayVec::new();

	key.extend(shortroomid.to_be_bytes());
	key.extend(parent.to_be_bytes());
	key.push(u8::from(tag));
	key.extend(child_ts.to_be_bytes());
	key.extend(child.to_be_bytes());
	key
}

/// Reads the child a stored `relatesto_typed` key names.
///
/// A key of any length but the writer's is not a relation row and gives `None`.
pub(crate) fn key_child(key: &[u8]) -> Option<PduId> {
	key.len().eq(&KEY_LEN).then(|| PduId {
		shortroomid: u64_from_u8(&key[..size_of::<ShortRoomId>()]),
		count: PduCount::from_unsigned(u64_from_u8(&key[CHILD_COUNT_OFFSET..])),
	})
}

/// Remove the `relatesto_typed` row for a redacted `m.replace` or `m.reference`
/// child. Storage hygiene for edits; correctness-critical for references, whose
/// read emits from the index value without loading the child. Call before the
/// child's content is stripped, while its relation fields are still readable.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "debug")]
pub async fn delete_typed_relation(&self, child_id: &RawPduId, child: &CanonicalJsonObject) {
	let Some(relates_to) = child
		.get("content")
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|content| content.get("m.relates_to"))
		.and_then(CanonicalJsonValue::as_object)
	else {
		return;
	};

	let tag = match relates_to
		.get("rel_type")
		.and_then(CanonicalJsonValue::as_str)
	{
		| Some("m.replace") => Tag::Replace,
		| Some("m.reference") => Tag::Reference,
		| _ => return,
	};

	let Some(parent) = relates_to
		.get("event_id")
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|parent| EventId::parse(parent).ok())
	else {
		return;
	};

	let Some(child_ts) = child
		.get("origin_server_ts")
		.and_then(CanonicalJsonValue::as_integer)
		.and_then(|ts| u64::try_from(i64::from(ts)).ok())
	else {
		return;
	};

	let child_count = child_id.pdu_count();
	let shortroomid = u64_from_u8(&child_id.shortroomid());

	let Ok(parent_count) = self
		.services
		.timeline
		.get_pdu_count(&parent)
		.await
	else {
		return;
	};

	let (PduCount::Normal(_), PduCount::Normal(_)) = (parent_count, child_count) else {
		return;
	};

	let key = key(shortroomid, parent_count, tag, child_ts, child_count);

	self.db.relatesto_typed.remove(key.as_slice());
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn delete_all_relatesto_typed_for_room(&self, room_id: &RoomId) -> Result {
	let Ok(shortroomid) = self.services.short.get_shortroomid(room_id).await else {
		return Ok(());
	};

	self.db
		.relatesto_typed
		.keys_prefix_raw(&shortroomid)
		.ignore_err()
		.ready_for_each(|key| {
			self.db.relatesto_typed.remove(key);
		})
		.await;

	Ok(())
}

pub(super) fn prefix(shortroomid: ShortRoomId, parent: PduCount, tag: Tag) -> Prefix {
	let mut prefix = ArrayVec::new();

	prefix.extend(shortroomid.to_be_bytes());
	prefix.extend(parent.to_be_bytes());
	prefix.push(u8::from(tag));
	prefix
}
