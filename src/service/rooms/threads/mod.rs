use std::{collections::BTreeMap, sync::Arc};

use futures::{Stream, StreamExt, TryFutureExt};
use ruma::{
	CanonicalJsonValue, EventId, OwnedEventId, OwnedUserId, RoomId, UserId,
	api::client::threads::get_threads::v1::IncludeThreads,
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
	trace,
	utils::{
		ReadyExt,
		stream::{TryIgnore, WidebandExt},
	},
};
use tuwunel_database::{Deserialized, Interfix, Map};

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
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				threadid_userids: args.db["threadid_userids"].clone(),
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

	pub async fn add_to_thread<E>(&self, root_event_id: &EventId, event: &E) -> Result
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

		let is_participant = move |participants: &[u8]| {
			participants
				.split(|&byte| byte == 0xFF)
				.any(|user| user == user_id.as_bytes())
		};

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
					.threadid_userids
					.rev_raw_stream_from(&current)
					.ignore_err()
					.map(|(key, participants)| (RawPduId::from(key), participants))
					.ready_take_while(move |(pdu_id, _)| {
						pdu_id.shortroomid() == current.shortroomid()
					})
					.ready_filter_map(move |(pdu_id, participants)| {
						(!participated || is_participant(participants))
							.then_some((pdu_id, user_id))
					})
					.wide_filter_map(async |(raw_pdu_id, user_id)| {
						let pdu_id: PduId = raw_pdu_id.into();
						let mut pdu = self
							.services
							.timeline
							.get_pdu_from_id(&raw_pdu_id)
							.await
							.ok()?;

						if pdu.sender() != user_id {
							pdu.as_mut_pdu().remove_transaction_id().ok();
						}

						Some((pdu_id.count, pdu))
					})
					.map(Ok)
			})
			.try_flatten_stream()
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

		self.get_participants(&root_id)
			.await
			.is_ok_and(|users| users.iter().any(|user| user == user_id))
	}

	pub(super) async fn delete_all_rooms_threads(&self, room_id: &RoomId) -> Result {
		let prefix = (room_id, Interfix);

		self.db
			.threadid_userids
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| {
				trace!("Removing key: {key:?}");
				self.db.threadid_userids.remove(key);
			})
			.await;

		Ok(())
	}
}
