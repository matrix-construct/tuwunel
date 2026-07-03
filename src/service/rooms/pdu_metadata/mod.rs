use std::sync::Arc;

use futures::{Stream, StreamExt, TryFutureExt, future::Either, pin_mut};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, OwnedUserId, RoomId, UserId,
	api::Direction,
	events::{reaction::ReactionEventContent, relation::RelationType, room::encrypted::Relation},
};
use serde::Deserialize;
use tuwunel_core::{
	PduId, Result,
	arrayvec::ArrayVec,
	implement, is_equal_to,
	matrix::{Event, Pdu, PduCount, RawPduId, event::RelationTypeEqual},
	result::LogErr,
	trace,
	utils::{
		BoolExt,
		stream::{ReadyExt, TryIgnore, WidebandExt, automatic_width},
		u64_from_u8,
	},
};
use tuwunel_database::{Interfix, Map};

use crate::rooms::short::ShortRoomId;

#[cfg(test)]
mod tests;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	tofrom_relation: Arc<Map>,
	relatesto_typed: Arc<Map>,
	referencedevents: Arc<Map>,
	softfailedeventids: Arc<Map>,
}

/// `relatesto_typed` rel_type discriminant, occupying one key byte between the
/// parent `RawPduId` and the child's ts. Stable on-disk format; the explicit
/// discriminants are permanent and must stay distinct.
#[derive(Clone, Copy)]
enum RelTag {
	Replace = 0x01,
	Reference = 0x02,
}

impl From<RelTag> for u8 {
	#[inline]
	fn from(tag: RelTag) -> Self {
		match tag {
			| RelTag::Replace => 0x01,
			| RelTag::Reference => 0x02,
		}
	}
}

/// `relatesto_typed` seek prefix: `shortroomid || parent_count || tag`.
const TYPED_PREFIX_LEN: usize = size_of::<u64>() * 2 + size_of::<u8>();

/// `relatesto_typed` key: the prefix followed by `child_ts || child_count`.
const TYPED_KEY_LEN: usize = TYPED_PREFIX_LEN + size_of::<u64>() * 2;

/// `relatesto_typed` key: byte offset of the child `PduCount` (the key tail).
const TYPED_CHILD_COUNT_OFFSET: usize = TYPED_KEY_LEN - size_of::<u64>();

/// Cap on the `m.reference` bundle chunk; /relations is the paginated fallback.
const REFERENCE_BUNDLE_MAX: usize = 100;

#[derive(Deserialize)]
struct ExtractRelatesTo {
	#[serde(rename = "m.relates_to")]
	relates_to: Relation,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data {
				tofrom_relation: args.db["tofrom_relation"].clone(),
				relatesto_typed: args.db["relatesto_typed"].clone(),
				referencedevents: args.db["referencedevents"].clone(),
				softfailedeventids: args.db["softfailedeventids"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
#[tracing::instrument(skip(self, from, to), level = "debug")]
pub fn add_relation(&self, from: PduCount, to: PduCount) {
	const BUFSIZE: usize = size_of::<u64>() * 2;

	match (from, to) {
		| (PduCount::Normal(from), PduCount::Normal(to)) => {
			let key: &[u64] = &[to, from];
			self.db
				.tofrom_relation
				.aput_raw::<BUFSIZE, _, _>(key, []);
		},
		| _ => {}, // TODO: Relations with backfilled pdus
	}
}

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
	let Some(tag) = rel_type_tag(&rel_type) else {
		return;
	};

	let Ok(parent_count) = self.services.timeline.get_pdu_count(parent).await else {
		return;
	};

	let (PduCount::Normal(_), PduCount::Normal(_)) = (parent_count, child_count) else {
		return; // backfilled relations are not indexed
	};

	let child_short = self
		.services
		.short
		.get_or_create_shorteventid(child.event_id())
		.await;

	let child_ts = u64::from(child.origin_server_ts().get());
	let key = typed_relation_key(shortroomid, parent_count, tag, child_ts, child_count);

	self.db
		.relatesto_typed
		.aput_raw::<TYPED_KEY_LEN, _, _>(key.as_slice(), child_short.to_be_bytes());
}

/// Query relations of an event to determine if matching any of the trailing
/// arguments. When all criteria are None the mere presence of a relation causes
/// this function to return true.
#[implement(Service)]
pub async fn event_has_relation(
	&self,
	event_id: &EventId,
	user_id: Option<&UserId>,
	rel_type: Option<&RelationType>,
	key: Option<&str>,
) -> bool {
	let Ok(pdu_id) = self.services.timeline.get_pdu_id(event_id).await else {
		return false;
	};

	self.has_relation(pdu_id.into(), user_id, rel_type, key)
		.await
}

/// Query relations of an event by PduId to determine if matching any of the
/// trailing arguments. When all criteria are None the mere presence of a
/// relation causes this function to return true.
#[implement(Service)]
pub async fn has_relation(
	&self,
	target: PduId,
	user_id: Option<&UserId>,
	rel_type: Option<&RelationType>,
	key: Option<&str>,
) -> bool {
	self.get_relations(target.shortroomid, target.count, None, Direction::Forward, None)
		.ready_filter(|(_, pdu)| user_id.is_none_or(is_equal_to!(pdu.sender())))
		.ready_filter(|(_, pdu)| {
			debug_assert!(
				key.is_none() || rel_type.is_none_or(is_equal_to!(&RelationType::Annotation)),
				"key argument only applies to Annotation type relations."
			);

			// When key is supplied we don't need to double-parse the content here and below.
			key.is_some() || rel_type
				.is_none_or(|rel_type| rel_type.relation_type_equal(&pdu))
		})
		.ready_filter(|(_, pdu)| {
			key.is_none_or(|key| {
				pdu.get_content::<ReactionEventContent>()
					.map(|content| content.relates_to.key == key)
					.unwrap_or(false)
			})
		})
		.ready_any(|_| true) // first match or false
		.await
}

/// MSC3440 `related_by_*`: whether any event relates to `target` with a
/// `rel_type` in `rel_types` and a `sender` in `senders`. An empty list is
/// unconstrained on that axis; a single relating event must satisfy both.
#[implement(Service)]
pub async fn has_incoming_relation(
	&self,
	target: PduId,
	senders: &[OwnedUserId],
	rel_types: &[RelationType],
) -> bool {
	self.get_relations(target.shortroomid, target.count, None, Direction::Forward, None)
		.ready_any(|(_, pdu)| {
			let sender = senders.is_empty() || senders.iter().any(is_equal_to!(pdu.sender()));

			let rel_type = rel_types.is_empty()
				|| rel_types
					.iter()
					.any(|rel_type| rel_type.relation_type_equal(&pdu));

			sender && rel_type
		})
		.await
}

#[implement(Service)]
pub fn get_relations<'a>(
	&'a self,
	shortroomid: ShortRoomId,
	target: PduCount,
	from: Option<PduCount>,
	dir: Direction,
	user_id: Option<&'a UserId>,
) -> impl Stream<Item = (PduCount, Pdu)> + Send + '_ {
	let target = target.to_be_bytes();
	let from = from
		.map(|from| from.saturating_inc(dir))
		.unwrap_or_else(|| match dir {
			| Direction::Backward => PduCount::max(),
			| Direction::Forward => PduCount::default(),
		})
		.to_be_bytes();

	let mut buf = ArrayVec::<u8, 16>::new();
	let start = {
		buf.extend(target);
		buf.extend(from);
		buf.as_slice()
	};

	match dir {
		| Direction::Backward => Either::Left(self.db.tofrom_relation.rev_raw_keys_from(start)),
		| Direction::Forward => Either::Right(self.db.tofrom_relation.raw_keys_from(start)),
	}
	.ignore_err()
	.ready_take_while(move |key| key.starts_with(&target))
	.map(|to_from| u64_from_u8(&to_from[8..16]))
	.map(PduCount::from_unsigned)
	.map(move |count| (user_id, shortroomid, count))
	.wide_filter_map(async |(user_id, shortroomid, count)| {
		let pdu_id: RawPduId = PduId { shortroomid, count }.into();
		self.services
			.timeline
			.get_pdu_from_id(&pdu_id)
			.map_ok(move |mut pdu| {
				if user_id.is_none_or(|user_id| pdu.sender() != user_id) {
					pdu.as_mut_pdu()
						.remove_transaction_id()
						.log_err()
						.ok();
				}

				(count, pdu)
			})
			.await
			.ok()
	})
}

/// Fold read-time bundled aggregations into a served event's `unsigned`,
/// per-requester. MSC3816: the stored `m.thread` bundle carries a shared
/// `current_user_participated`, recomputed here for `sender_user`. MSC3925:
/// when `bundle_edit_relations` is enabled, the newest `m.replace` edit is
/// folded in as the full replacement event. MSC3267: when
/// `bundle_reference_relations` is enabled, the `m.reference` children are
/// folded in as a `{ chunk: [{ event_id }] }` summary. The thread presence gate
/// keeps the common no-bundle case to a substring scan; the edit and reference
/// folds are skipped unless enabled.
#[implement(Service)]
pub async fn bundle_aggregations(&self, sender_user: &UserId, mut pdu: Pdu) -> Pdu {
	// MSC4025: an erased sender's event serves as the pruned clone, and a
	// pruned event carries no aggregations.
	if let Some(pruned) = self
		.services
		.state_accessor
		.erased_view(sender_user, &pdu)
		.await
	{
		return pruned;
	}

	let has_thread = pdu
		.unsigned()
		.is_some_and(|unsigned| unsigned.get().contains("m.thread"));

	if has_thread {
		let participated = self
			.services
			.threads
			.user_participated(pdu.event_id(), sender_user)
			.await;

		pdu.set_thread_participated(participated)
			.log_err()
			.ok();

		self.erase_thread_latest(sender_user, &mut pdu)
			.await;
	}

	let replacement = self
		.services
		.server
		.config
		.bundle_edit_relations
		.then_async(|| self.newest_replacement(&pdu))
		.await
		.flatten();

	if let Some(replacement) = replacement
		&& !self
			.services
			.state_accessor
			.erased_for(sender_user, &replacement)
			.await
	{
		pdu.set_replacement_bundle(&replacement.to_format())
			.log_err()
			.ok();
	}

	let references = self
		.services
		.server
		.config
		.bundle_reference_relations
		.then_async(|| self.references(&pdu))
		.await
		.unwrap_or_default();

	if !references.is_empty() {
		pdu.set_reference_bundle(&references)
			.log_err()
			.ok();
	}

	pdu
}

/// MSC4025: the stored thread bundle carries a full `latest_event` of any
/// sender; an erased hit swaps in the pruned form for this recipient. The
/// event load and membership check run only on the erased hit.
#[implement(Service)]
async fn erase_thread_latest(&self, sender_user: &UserId, pdu: &mut Pdu) {
	let Some((event_id, sender)) = pdu.thread_latest_event() else {
		return;
	};

	if !self.services.users.is_erased(&sender).await {
		return;
	}

	let Ok(latest) = self.services.timeline.get_pdu(&event_id).await else {
		return;
	};

	if let Some(pruned) = self
		.services
		.state_accessor
		.erased_view(sender_user, &latest)
		.await
	{
		pdu.set_thread_latest_event(&pruned.to_format())
			.log_err()
			.ok();
	}
}

/// MSC3925: the newest `m.replace` edit of `parent` as a full event, or `None`
/// when `parent` is redacted or has no valid edit. An edit counts only when it
/// shares the parent's sender and type and is not itself redacted; newest is by
/// `origin_server_ts`, which the typed index sorts on.
#[implement(Service)]
async fn newest_replacement(&self, parent: &Pdu) -> Option<Pdu> {
	if parent.is_redacted() {
		return None;
	}

	let parent_id: PduId = self
		.services
		.timeline
		.get_pdu_id(parent.event_id())
		.map_ok(Into::into)
		.await
		.ok()?;

	let replacements = self.replacement_children(parent, parent_id);

	pin_mut!(replacements);
	replacements.next().await
}

/// Stream `parent`'s valid `m.replace` children, newest `origin_server_ts`
/// first, from the typed index. A child counts only when it shares the parent's
/// sender and type and is not itself redacted.
#[implement(Service)]
fn replacement_children<'a>(
	&'a self,
	parent: &'a Pdu,
	parent_id: PduId,
) -> impl Stream<Item = Pdu> + Send + 'a {
	let shortroomid = parent_id.shortroomid;
	let prefix = typed_relation_prefix(shortroomid, parent_id.count, RelTag::Replace);

	let mut seek = ArrayVec::<u8, TYPED_KEY_LEN>::new();
	seek.extend(prefix.iter().copied());
	seek.extend([u8::MAX; size_of::<u64>() * 2]);

	self.db
		.relatesto_typed
		.rev_raw_keys_from(seek.as_slice())
		.ignore_err()
		.ready_take_while(move |key| key.starts_with(&prefix))
		.map(|key| u64_from_u8(&key[TYPED_CHILD_COUNT_OFFSET..TYPED_KEY_LEN]))
		.map(PduCount::from_unsigned)
		.map(move |count| (shortroomid, count))
		.filter_map(async |(shortroomid, count)| {
			let child_id: RawPduId = PduId { shortroomid, count }.into();
			self.services
				.timeline
				.get_pdu_from_id(&child_id)
				.await
				.ok()
				.filter(|child| !child.is_redacted())
				.filter(|child| child.sender() == parent.sender())
				.filter(|child| child.kind() == parent.kind())
		})
}

/// MSC2675/MSC3267: the event ids of `parent`'s `m.reference` children, oldest
/// first, from the typed index, capped at `REFERENCE_BUNDLE_MAX`. Empty when
/// `parent` is redacted or unreferenced. The ids come from the index value (the
/// child shorteventid) without loading the children, so the chunk is filtered
/// for neither ignored users nor history visibility. The ignored-user posture
/// matches the /relations endpoint, which also does not filter relation
/// children by ignored sender; the history-visibility posture matches the
/// thread and edit bundles and is less strict than /relations, which does
/// filter children by visibility.
#[implement(Service)]
async fn references(&self, parent: &Pdu) -> Vec<OwnedEventId> {
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
		.take(REFERENCE_BUNDLE_MAX)
		.collect()
		.await
}

/// Stream the event ids of `parent_id`'s `m.reference` children, oldest first,
/// from the typed index, resolving each row value (the child shorteventid) to
/// an event id with no PDU load.
#[implement(Service)]
fn referenced_children<'a>(
	&'a self,
	parent_id: PduId,
) -> impl Stream<Item = OwnedEventId> + Send + 'a {
	let prefix = typed_relation_prefix(parent_id.shortroomid, parent_id.count, RelTag::Reference);
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
	for prev in event_ids {
		let key = (room_id, prev);
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
		| Some("m.replace") => RelTag::Replace,
		| Some("m.reference") => RelTag::Reference,
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

	let key = typed_relation_key(shortroomid, parent_count, tag, child_ts, child_count);

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

/// Rebuild `relatesto_typed` from every stored PDU. Run once at startup behind
/// a `global` marker, and on demand from the admin command. Clears first so a
/// partial or stale index is replaced wholesale.
#[implement(Service)]
pub async fn rebuild_typed_relations(&self) -> Result {
	self.db.relatesto_typed.clear().await;

	let pduid_pdu = self.services.db["pduid_pdu"].clone();

	pduid_pdu
		.raw_stream()
		.ignore_err()
		.ready_filter_map(|(key, value)| {
			let pdu_id = RawPduId::from(key);
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
async fn index_pdu_relations(&self, pdu_id: RawPduId, pdu: &Pdu) {
	let Ok(content) = pdu.get_content::<ExtractRelatesTo>() else {
		return;
	};

	let (rel_type, parent) = match content.relates_to {
		| Relation::Replacement(replacement) => (RelationType::Replacement, replacement.event_id),
		| Relation::Reference(reference) => (RelationType::Reference, reference.event_id),
		| _ => return,
	};

	let shortroomid = u64_from_u8(&pdu_id.shortroomid());

	self.add_typed_relation(shortroomid, pdu_id.pdu_count(), &parent, pdu, rel_type)
		.await;
}

fn rel_type_tag(rel_type: &RelationType) -> Option<RelTag> {
	match rel_type {
		| RelationType::Replacement => Some(RelTag::Replace),
		| RelationType::Reference => Some(RelTag::Reference),
		| _ => None,
	}
}

fn typed_relation_prefix(
	shortroomid: ShortRoomId,
	parent: PduCount,
	tag: RelTag,
) -> ArrayVec<u8, TYPED_PREFIX_LEN> {
	let mut buf = ArrayVec::new();
	buf.extend(shortroomid.to_be_bytes());
	buf.extend(parent.to_be_bytes());
	buf.push(u8::from(tag));
	buf
}

fn typed_relation_key(
	shortroomid: ShortRoomId,
	parent: PduCount,
	tag: RelTag,
	child_ts: u64,
	child: PduCount,
) -> ArrayVec<u8, TYPED_KEY_LEN> {
	let mut buf = ArrayVec::new();
	buf.extend(shortroomid.to_be_bytes());
	buf.extend(parent.to_be_bytes());
	buf.push(u8::from(tag));
	buf.extend(child_ts.to_be_bytes());
	buf.extend(child.to_be_bytes());
	buf
}
