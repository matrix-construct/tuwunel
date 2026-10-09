use std::{collections::BTreeMap, pin::pin, sync::Arc};

use futures::{Stream, StreamExt, TryFutureExt, TryStreamExt, future::join3};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, OwnedUserId, RoomId, UInt,
	UserId,
	api::{Direction, client::threads::get_threads::v1::IncludeThreads},
	events::{
		AnySyncMessageLikeEvent, TimelineEventType, relation::RelationType,
		room::encrypted::Relation,
	},
	serde::Raw,
	uint,
};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{
	Event, Result, err, implement,
	matrix::{
		Pdu,
		pdu::{PduCount, PduEvent, PduId, RawPduId},
	},
	utils::{
		BoolExt, ReadyExt,
		result::{LogErr, NotFound},
		stream::{TryIgnore, TryReadyExt, WidebandExt, automatic_width},
	},
};
use tuwunel_database::{Deserialized, Map, Txn};

use crate::rooms::{pdu_metadata::RelationQuery, timeline::ExtractRelatesTo};

#[cfg(test)]
mod tests;

/// Maximum relation hops walked when resolving thread membership, per
/// the Matrix v1.4 spec recommendation (also MSC3771/MSC3773).
const MAX_THREAD_HOPS: usize = 3;

#[derive(Deserialize)]
struct ExtractThreadRelation {
	#[serde(rename = "m.relates_to")]
	relates_to: ThreadRelation,
}

#[derive(Deserialize)]
struct ThreadRelation {
	rel_type: RelationType,
	event_id: OwnedEventId,
}

fn canonical_object_field<'a>(
	object: &'a mut CanonicalJsonObject,
	field: &str,
) -> &'a mut CanonicalJsonObject {
	if !matches!(object.get(field), Some(CanonicalJsonValue::Object(_))) {
		object.insert(field.into(), CanonicalJsonValue::Object(BTreeMap::new()));
	}

	let Some(CanonicalJsonValue::Object(value)) = object.get_mut(field) else {
		unreachable!("canonical object field was initialized as an object");
	};

	value
}

/// Persist a latest event whose embedded sender and event ID come from the
/// same validated event used to index the thread activity.
fn update_thread_bundle<E>(unsigned: &mut CanonicalJsonObject, event: &E)
where
	E: Event,
{
	let latest_event = event.to_sync_message_like_without_unsigned();
	update_thread_bundle_raw(unsigned, &latest_event);
}

fn update_thread_bundle_raw(
	unsigned: &mut CanonicalJsonObject,
	latest_event: &Raw<AnySyncMessageLikeEvent>,
) {
	let relations = canonical_object_field(unsigned, "m.relations");
	let thread = canonical_object_field(relations, "m.thread");
	let count = thread
		.get("count")
		.cloned()
		.and_then(|count| serde_json::from_value::<UInt>(count.into()).ok());

	let count = count.map_or_else(|| uint!(1), |count| count.saturating_add(uint!(1)));
	let latest_event = serde_json::from_str(latest_event.json().get())
		.expect("thread latest event should be canonical JSON");

	thread.insert("latest_event".into(), latest_event);
	thread.insert(
		"count".into(),
		json!(count)
			.try_into()
			.expect("thread count is canonical JSON"),
	);

	if !matches!(thread.get("current_user_participated"), Some(CanonicalJsonValue::Bool(_))) {
		thread.insert("current_user_participated".into(), CanonicalJsonValue::Bool(true));
	}
}

pub struct Service {
	db: Data,
	services: Arc<crate::services::OnceServices>,
}

pub(super) struct Data {
	threadid_userids: Arc<Map>,
	threadactivityid_rootid: Arc<Map>,
	threadrootid_latestcount: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				threadid_userids: args.db["threadid_userids"].clone(),
				threadactivityid_rootid: args.db["threadactivityid_rootid"].clone(),
				threadrootid_latestcount: args.db["threadrootid_latestcount"].clone(),
			},
			services: args.services.clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Resolves the thread root for `event` by walking up `m.relates_to`
	/// links, bounded at `MAX_THREAD_HOPS`. Returns `None` for events
	/// that belong to the main timeline. Redaction events carry no
	/// `m.relates_to` of their own; their thread is resolved from the
	/// redacted target event per MSC3771/MSC3773.
	pub async fn get_thread_id<E>(&self, event: &E) -> Option<OwnedEventId>
	where
		E: Event,
	{
		let initial = match event.get_content::<ExtractThreadRelation>() {
			| Ok(t) => Some(t.relates_to),
			| Err(_) => self.relates_to_via_redaction_target(event).await,
		};

		let mut relates_to = initial?;

		for _ in 0..MAX_THREAD_HOPS {
			if relates_to.rel_type == RelationType::Thread {
				return Some(relates_to.event_id);
			}

			relates_to = self
				.services
				.timeline
				.get_pdu(&relates_to.event_id)
				.await
				.ok()?
				.get_content::<ExtractThreadRelation>()
				.ok()?
				.relates_to;
		}

		None
	}

	/// Resolve a redaction event's thread by looking through to the
	/// redacted target. Returns `None` for non-redaction events and for
	/// redactions whose target is unknown or carries no thread relation.
	async fn relates_to_via_redaction_target<E>(&self, event: &E) -> Option<ThreadRelation>
	where
		E: Event,
	{
		if *event.kind() != TimelineEventType::RoomRedaction {
			return None;
		}

		let room_rules = self
			.services
			.state
			.get_room_version_rules(event.room_id())
			.await
			.ok()?;

		let target_id = event.redacts_id(&room_rules)?;

		self.services
			.timeline
			.get_pdu(&target_id)
			.await
			.ok()?
			.get_content::<ExtractThreadRelation>()
			.ok()
			.map(|t| t.relates_to)
	}

	/// `get_thread_id` for an event referenced by id; events missing
	/// locally resolve to `None` (the main timeline).
	pub async fn get_thread_id_for_event(&self, event_id: &EventId) -> Option<OwnedEventId> {
		let pdu = self
			.services
			.timeline
			.get_pdu(event_id)
			.await
			.ok()?;

		self.get_thread_id(&pdu).await
	}

	pub async fn add_to_thread<E>(
		&self,
		root_event_id: &EventId,
		pdu_id: RawPduId,
		event: &E,
	) -> Result
	where
		E: Event,
	{
		let root_id = self
			.services
			.timeline
			.get_pdu_id(root_event_id)
			.await
			.map_err(|e| {
				err!(Request(InvalidParam("Invalid event_id in thread message: {e:?}")))
			})?;

		let root_pdu = self
			.services
			.timeline
			.get_pdu_from_id(&root_id)
			.await
			.map_err(|e| err!(Request(InvalidParam("Thread root not found: {e:?}"))))?;

		if root_pdu.room_id() != event.room_id() {
			return Ok(());
		}

		let mut root_pdu_json = self
			.services
			.timeline
			.get_pdu_json_from_id(&root_id)
			.await
			.map_err(|e| err!(Request(InvalidParam("Thread root pdu not found: {e:?}"))))?;

		let mut users = self
			.get_participants(&root_id)
			.await
			.unwrap_or_else(|_| vec![root_pdu.sender().to_owned()]);

		users.push(event.sender().to_owned());

		let mut txn = self.services.db.txn();

		self.update_participants(&mut txn, &root_id, &users);

		let count = pdu_id.pdu_count();

		if matches!(count, PduCount::Normal(_)) {
			txn.insert_raw(&self.db.threadactivityid_rootid, pdu_id, root_id);
			txn.insert_raw(&self.db.threadrootid_latestcount, root_id, count.to_be_bytes());
		}

		if let CanonicalJsonValue::Object(unsigned) = root_pdu_json
			.entry("unsigned".into())
			.or_insert_with(|| CanonicalJsonValue::Object(BTreeMap::default()))
		{
			update_thread_bundle(unsigned, event);

			self.services
				.timeline
				.stage_replace_pdu(&mut txn, &root_id, &root_pdu_json);
		}

		txn.execute();
		Ok(())
	}

	/// Returns the root row changed by redacting a counted thread reply.
	///
	/// The caller holds the room lock while the count decreases and the latest
	/// reply is replaced. A bounded miss omits the latest event while preserving
	/// the count; the whole bundle is removed when no reply remains.
	/// Missing roots and unchanged summaries return `None`; storage errors are logged.
	pub async fn redacted_reply_root(
		&self,
		root_event_id: &EventId,
		reply_id: &RawPduId,
		reply_event_id: &EventId,
	) -> Option<(RawPduId, CanonicalJsonObject)> {
		matches!(reply_id.pdu_count(), PduCount::Normal(_)).into_option()?;

		let root_id = self
			.services
			.timeline
			.get_pdu_id(root_event_id)
			.await
			.optional()
			.log_err()
			.ok()??;

		root_id
			.shortroomid()
			.eq(&reply_id.shortroomid())
			.into_option()?;

		let root = self.summary_root(&root_id).await?;
		let count = thread_count(&root).and_then(|count| count.checked_sub(uint!(1)));
		let replace =
			thread_latest(&root).is_some_and(|latest| latest == reply_event_id.as_str());

		let latest = replace
			.then_async(|| self.latest_thread_reply(root_id, reply_event_id))
			.await;

		let latest = latest.map(|latest| latest.log_err().ok().flatten());

		let changed = count.is_some() || latest.is_some();

		changed.into_option()?;

		let root = set_thread_count(root, count);
		let root = match (count, latest) {
			| (Some(count), _) if count == uint!(0) => set_thread_latest(root, None),
			| (_, None) => root,
			| (_, Some(None)) => omit_thread_latest(root),
			| (_, Some(Some(latest))) => set_thread_latest(root, Some(latest)),
		};

		Some((root_id, root))
	}

	pub fn threads_until<'a>(
		&'a self,
		user_id: &'a UserId,
		room_id: &'a RoomId,
		count: PduCount,
		include: &'a IncludeThreads,
	) -> impl Stream<Item = Result<(PduCount, PduEvent)>> + Send {
		let participated = matches!(include, IncludeThreads::Participated);

		self.services
			.short
			.get_shortroomid(room_id)
			.map_ok(move |shortroomid| PduId {
				shortroomid,
				count: count.saturating_sub(1),
			})
			.map_ok(Into::into)
			.map_ok(move |current: RawPduId| {
				self.db
					.threadactivityid_rootid
					.rev_raw_stream_from(&current)
					.ignore_err()
					.map(|(key, root_id)| (RawPduId::from(key), RawPduId::from(root_id)))
					.ready_take_while(move |(activity_id, _)| {
						activity_id.shortroomid() == current.shortroomid()
					})
					.map(move |(activity_id, root_id)| {
						(activity_id, root_id, user_id, participated)
					})
					.wide_filter_map(async |(activity_id, root_id, user_id, participated)| {
						self.live_thread(user_id, participated, activity_id, root_id)
							.await
					})
					.map(Ok)
			})
			.try_flatten_stream()
	}

	/// Resolve one activity row to its thread root, skipping and reaping rows
	/// the validity pointer has left behind.
	async fn live_thread(
		&self,
		user_id: &UserId,
		participated: bool,
		activity_id: RawPduId,
		root_id: RawPduId,
	) -> Option<(PduCount, PduEvent)> {
		let count = activity_id.pdu_count();

		let pointer = self
			.db
			.threadrootid_latestcount
			.get(&root_id)
			.await
			.deserialized()
			.map(PduCount::from_unsigned)
			.ok()?;

		if count != pointer {
			// A row ahead of the pointer is a write in flight; only rows behind
			// the pointer are dead and safe to reap.
			if count < pointer {
				self.db
					.threadactivityid_rootid
					.remove(&activity_id);
			}

			return None;
		}

		if participated && !self.is_participant(&root_id, user_id).await {
			return None;
		}

		let mut pdu = self
			.services
			.timeline
			.get_pdu_from_id(&root_id)
			.await
			.ok()?;

		pdu.remove_transaction_id_unless_sender(Some(user_id))
			.ok()?;

		Some((count, pdu))
	}

	async fn is_participant(&self, root_id: &RawPduId, user_id: &UserId) -> bool {
		self.db
			.threadid_userids
			.get(root_id)
			.await
			.is_ok_and(|participants| {
				participants
					.split(|&byte| byte == 0xFF)
					.any(|user| user == user_id.as_bytes())
			})
	}

	pub(super) fn update_participants(
		&self,
		txn: &mut Txn,
		root_id: &RawPduId,
		participants: &[OwnedUserId],
	) {
		let users = participants
			.iter()
			.map(|user| user.as_bytes())
			.collect::<Vec<_>>()
			.join(&[0xFF][..]);

		txn.insert_raw(&self.db.threadid_userids, root_id, &users);
	}

	pub(super) async fn get_participants(&self, root_id: &RawPduId) -> Result<Vec<OwnedUserId>> {
		self.db
			.threadid_userids
			.get(root_id)
			.await
			.deserialized()
	}

	/// MSC3816: whether `user_id` has participated in the thread rooted at
	/// `root_event_id`, having sent the root event or a threaded reply to it.
	pub async fn user_participated(&self, root_event_id: &EventId, user_id: &UserId) -> bool {
		let Ok(root_id) = self
			.services
			.timeline
			.get_pdu_id(root_event_id)
			.await
		else {
			return false;
		};

		self.is_participant(&root_id, user_id).await
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub(super) async fn delete_all_rooms_threads(&self, room_id: &RoomId) -> Result {
		let Ok(shortroomid) = self.services.short.get_shortroomid(room_id).await else {
			return Ok(());
		};

		join3(
			self.db.threadid_userids.del_prefix(&shortroomid),
			self.db
				.threadactivityid_rootid
				.del_prefix(&shortroomid),
			self.db
				.threadrootid_latestcount
				.del_prefix(&shortroomid),
		)
		.await;

		Ok(())
	}

	/// Rebuild the thread activity index from every thread root. Run once at
	/// startup behind a `global` marker, and on demand from the admin command.
	/// Clears first so a partial or stale index is replaced wholesale.
	pub async fn rebuild_thread_activity(&self) -> Result {
		self.db.threadactivityid_rootid.clear().await;
		self.db.threadrootid_latestcount.clear().await;

		self.db
			.threadid_userids
			.raw_keys()
			.ignore_err()
			.map(RawPduId::from)
			.for_each_concurrent(automatic_width(), async |root_id| {
				self.index_thread_activity(root_id).await;
			})
			.await;

		Ok(())
	}

	async fn index_thread_activity(&self, root_id: RawPduId) {
		let root: PduId = root_id.into();

		let replies = self
			.services
			.pdu_metadata
			.get_relations(root.shortroomid, root.count, None, Direction::Backward, None)
			.ready_filter_map(|(count, pdu)| {
				pdu.get_content()
					.is_ok_and(|content: ExtractThreadRelation| {
						content.relates_to.rel_type == RelationType::Thread
					})
					.then_some(count)
			});

		let mut replies = pin!(replies);

		let latest = replies.next().await.unwrap_or(root.count);

		let activity_id: RawPduId = PduId {
			shortroomid: root.shortroomid,
			count: latest,
		}
		.into();

		let mut txn = self.services.db.txn();

		txn.insert_raw(&self.db.threadactivityid_rootid, activity_id, root_id);
		txn.insert_raw(&self.db.threadrootid_latestcount, root_id, latest.to_be_bytes());
		txn.execute();
	}

	/// Rebuilds stored thread counts and removes redacted latest replies.
	///
	/// Each root is locked and walked once; an unredacted stored latest is kept,
	/// and a backfilled root keeps its count. Returns the numbers of rewritten
	/// and failed roots, skipping missing bundles.
	#[tracing::instrument(skip_all, level = "debug")]
	pub async fn rebuild_thread_summaries(&self) -> (usize, usize) {
		// get_relations already fans out per root, so a concurrent outer stage nests fan-outs.
		self.db
			.threadid_userids
			.raw_keys()
			.ignore_err()
			.map(RawPduId::from)
			.then(async |root_id| {
				self.rebuild_thread_summary(root_id)
					.await
					.log_err()
			})
			.ready_fold((0_usize, 0_usize), |(changed, failed), result| match result {
				| Ok(rewritten) => (changed.saturating_add(usize::from(rewritten)), failed),
				| Err(_) => (changed, failed.saturating_add(1)),
			})
			.await
	}
}

#[implement(Service)]
async fn summary_root(&self, root_id: &RawPduId) -> Option<CanonicalJsonObject> {
	self.try_summary_root(root_id)
		.await
		.log_err()
		.ok()?
}

#[implement(Service)]
async fn try_summary_root(&self, root_id: &RawPduId) -> Result<Option<CanonicalJsonObject>> {
	self.services
		.timeline
		.get_pdu_json_from_id(root_id)
		.await
		.optional()
}

#[implement(Service)]
async fn latest_thread_reply(
	&self,
	root_id: RawPduId,
	excluding: &EventId,
) -> Result<Option<CanonicalJsonValue>> {
	let limit = self
		.services
		.server
		.config
		.thread_latest_reply_search_limit;

	let replies = self
		.thread_replies(root_id, limit)
		.ready_try_filter(|(_, pdu)| pdu.event_id != excluding);

	pin!(replies)
		.try_next()
		.await
		.map(|reply| reply.and_then(|(_, pdu)| thread_reply_json(&pdu)))
}

#[implement(Service)]
fn thread_replies(
	&self,
	root_id: RawPduId,
	limit: usize,
) -> impl Stream<Item = Result<(PduCount, Pdu)>> + Send + '_ {
	let PduId { shortroomid, count } = root_id.into();

	self.services
		.pdu_metadata
		.try_get_relations_limited(RelationQuery {
			shortroomid,
			target: count,
			from: None,
			dir: Direction::Backward,
			user_id: None,
			limit,
		})
		.ready_try_filter(|(_, pdu)| !pdu.is_redacted())
		.ready_try_filter(|(_, pdu)| is_thread_reply(pdu))
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
async fn rebuild_thread_summary(&self, root_id: RawPduId) -> Result<bool> {
	let Some(pdu) = self.try_summary_root(&root_id).await? else {
		return Ok(false);
	};

	let Some(_) = thread_bundle(&pdu) else { return Ok(false) };
	let room_id: &RoomId = pdu.get("room_id").try_into()?;
	let _lock = self.services.state.mutex.lock(room_id).await;
	let Some(root) = self.try_summary_root(&root_id).await? else {
		return Ok(false);
	};

	let Some(_) = thread_bundle(&root) else { return Ok(false) };

	let (count, latest) = self
		.thread_replies(root_id, usize::MAX)
		.ready_try_fold((0_usize, None), |(count, latest), (_, pdu)| {
			Ok((count.saturating_add(1), latest.or(Some(pdu))))
		})
		.await?;

	// A backfilled root keeps its count; its replies are not in the relation index.
	let count = matches!(root_id.pdu_count(), PduCount::Normal(_))
		.and_then(|| UInt::try_from(count).ok())
		.or_else(|| thread_count(&root));

	let replace = match thread_latest(&root).and_then(|id| <&EventId>::try_from(id).ok()) {
		| None => false,
		| Some(latest) => self
			.services
			.timeline
			.get_pdu(latest)
			.await
			.optional()?
			.is_none_or(|pdu| pdu.is_redacted()),
	};

	let changed = replace || thread_count(&root) != count;

	if !changed {
		return Ok(false);
	}

	let latest = replace.then(|| latest.as_ref().and_then(thread_reply_json));
	let root = set_thread_count(root, count);
	let root = match latest {
		| None => root,
		| Some(latest) => set_thread_latest(root, latest),
	};

	self.services
		.timeline
		.replace_pdu(&root_id, &root)
		.await?;

	Ok(true)
}

fn thread_count(root: &CanonicalJsonObject) -> Option<UInt> {
	UInt::try_from(i64::from(thread_bundle(root)?.get("count")?.as_integer()?)).ok()
}

fn thread_bundle(root: &CanonicalJsonObject) -> Option<&CanonicalJsonObject> {
	["unsigned", "m.relations", "m.thread"]
		.into_iter()
		.try_fold(root, |object, field| object.get(field)?.as_object())
}

fn thread_latest(root: &CanonicalJsonObject) -> Option<&str> {
	thread_bundle(root)?
		.get("latest_event")?
		.as_object()?
		.get("event_id")?
		.as_str()
}

fn set_thread_count(mut root: CanonicalJsonObject, count: Option<UInt>) -> CanonicalJsonObject {
	if let Some((thread, count)) = thread_bundle_mut(&mut root).zip(count) {
		thread.insert("count".into(), CanonicalJsonValue::Integer(count.into()));
	}

	root
}

/// Borrows the stored thread bundle for a summary rewrite.
///
/// Missing or non-object components leave the root unchanged.
pub(crate) fn thread_bundle_mut(
	root: &mut CanonicalJsonObject,
) -> Option<&mut CanonicalJsonObject> {
	["unsigned", "m.relations", "m.thread"]
		.into_iter()
		.try_fold(root, |object, field| object.get_mut(field)?.as_object_mut())
}

fn set_thread_latest(
	mut root: CanonicalJsonObject,
	latest: Option<CanonicalJsonValue>,
) -> CanonicalJsonObject {
	let Some(latest) = latest else {
		return remove_thread_bundle(root);
	};

	if let Some(thread) = thread_bundle_mut(&mut root) {
		thread.insert("latest_event".into(), latest);
	}

	root
}

fn omit_thread_latest(mut root: CanonicalJsonObject) -> CanonicalJsonObject {
	if thread_count(&root).is_none_or(|count| count == uint!(0)) {
		return remove_thread_bundle(root);
	}

	if let Some(thread) = thread_bundle_mut(&mut root) {
		thread.remove("latest_event");
	}

	root
}

fn remove_thread_bundle(mut root: CanonicalJsonObject) -> CanonicalJsonObject {
	let Some(unsigned) = root
		.get_mut("unsigned")
		.and_then(CanonicalJsonValue::as_object_mut)
	else {
		return root;
	};

	let Some(relations) = unsigned
		.get_mut("m.relations")
		.and_then(CanonicalJsonValue::as_object_mut)
	else {
		return root;
	};

	relations.remove("m.thread");
	if relations.is_empty() {
		unsigned.remove("m.relations");
	}

	root
}

fn thread_reply_json(pdu: &Pdu) -> Option<CanonicalJsonValue> {
	serde_json::from_str(
		pdu.to_sync_message_like_without_unsigned()
			.json()
			.get(),
	)
	.ok()
}

fn is_thread_reply(pdu: &impl Event) -> bool {
	pdu.get_content()
		.is_ok_and(|content: ExtractRelatesTo| matches!(content.relates_to, Relation::Thread(_)))
}

/// Returns the thread root named by borrowed event content.
///
/// Uses the same relation shape as the timeline append path, including legacy replies.
pub(crate) fn thread_root(content: &CanonicalJsonValue) -> Option<OwnedEventId> {
	let content: ExtractRelatesTo =
		serde_json::from_value(serde_json::to_value(content).ok()?).ok()?;

	match content.relates_to {
		| Relation::Thread(thread) => Some(thread.event_id),
		| _ => None,
	}
}
