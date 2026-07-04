use std::{collections::BTreeMap, pin::pin, sync::Arc};

use futures::{Stream, StreamExt, TryFutureExt, future::join3};
use ruma::{
	CanonicalJsonValue, EventId, OwnedEventId, OwnedUserId, RoomId, UserId,
	api::{Direction, client::threads::get_threads::v1::IncludeThreads},
	events::{
		TimelineEventType,
		relation::{BundledThread, RelationType},
	},
	uint,
};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{
	Event, Result, err,
	matrix::pdu::{PduCount, PduEvent, PduId, RawPduId},
	utils::{
		ReadyExt,
		stream::{TryIgnore, WidebandExt, automatic_width},
	},
};
use tuwunel_database::{Deserialized, Map};

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

		// Record participants before the bundle so a concurrent read never sees the
		// bundle with a stale participant set (MSC3816 current_user_participated).
		self.update_participants(&root_id, &users)?;

		let count = pdu_id.pdu_count();

		// The activity row lands before the pointer write that marks it live.
		if matches!(count, PduCount::Normal(_)) {
			self.db
				.threadactivityid_rootid
				.insert(&pdu_id, root_id);

			self.db
				.threadrootid_latestcount
				.insert(&root_id, count.to_be_bytes());
		}

		if let CanonicalJsonValue::Object(unsigned) = root_pdu_json
			.entry("unsigned".into())
			.or_insert_with(|| CanonicalJsonValue::Object(BTreeMap::default()))
		{
			if let Some(mut relations) = unsigned
				.get("m.relations")
				.and_then(|r| r.as_object())
				.and_then(|r| r.get("m.thread"))
				.and_then(|relations| {
					serde_json::from_value::<BundledThread>(relations.clone().into()).ok()
				}) {
				// Thread already existed
				relations.count = relations.count.saturating_add(uint!(1));
				relations.latest_event = event.to_format();

				let content = serde_json::to_value(relations).expect("to_value always works");

				unsigned.insert(
					"m.relations".into(),
					json!({ "m.thread": content })
						.try_into()
						.expect("thread is valid json"),
				);
			} else {
				// New thread
				let relations = BundledThread {
					latest_event: event.to_format(),
					count: uint!(1),
					current_user_participated: true,
				};

				let content = serde_json::to_value(relations).expect("to_value always works");

				unsigned.insert(
					"m.relations".into(),
					json!({ "m.thread": content })
						.try_into()
						.expect("thread is valid json"),
				);
			}

			self.services
				.timeline
				.replace_pdu(&root_id, &root_pdu_json)
				.await?;
		}

		Ok(())
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

		if pdu.sender() != user_id {
			pdu.as_mut_pdu().remove_transaction_id().ok();
		}

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
		root_id: &RawPduId,
		participants: &[OwnedUserId],
	) -> Result {
		let users = participants
			.iter()
			.map(|user| user.as_bytes())
			.collect::<Vec<_>>()
			.join(&[0xFF][..]);

		self.db.threadid_userids.insert(root_id, &users);

		Ok(())
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

		self.db
			.threadactivityid_rootid
			.insert(&activity_id, root_id);

		self.db
			.threadrootid_latestcount
			.insert(&root_id, latest.to_be_bytes());
	}
}
