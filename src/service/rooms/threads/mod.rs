use std::{collections::BTreeMap, sync::Arc};

use futures::{Stream, StreamExt};
use ruma::{
	CanonicalJsonValue, EventId, OwnedUserId, RoomId, UserId,
	api::client::threads::get_threads::v1::IncludeThreads, events::relation::BundledThread, uint,
};
use serde_json::json;
use tuwunel_core::{
	Result, err,
	matrix::pdu::{PduCount, PduEvent, PduId, RawPduId},
	utils::{
		ReadyExt,
		stream::{TryIgnore, WidebandExt},
	},
};
use tuwunel_database::{Deserialized, Map};

use crate::{Dep, rooms, rooms::short::ShortRoomId};

pub struct Service {
	db: Data,
	services: Services,
}

struct Services {
	short: Dep<rooms::short::Service>,
	timeline: Dep<rooms::timeline::Service>,
}

pub(super) struct Data {
	threadid_userids: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				threadid_userids: args.db["threadid_userids"].clone(),
			},
			services: Services {
				short: args.depend::<rooms::short::Service>("rooms::short"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
			},
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

impl Service {
	pub async fn add_to_thread(&self, root_event_id: &EventId, pdu: &PduEvent) -> Result<()> {
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

		if let CanonicalJsonValue::Object(unsigned) = root_pdu_json
			.entry("unsigned".to_owned())
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
				relations.latest_event = pdu.to_message_like_event();

				let content = serde_json::to_value(relations).expect("to_value always works");

				unsigned.insert(
					"m.relations".to_owned(),
					json!({ "m.thread": content })
						.try_into()
						.expect("thread is valid json"),
				);
			} else {
				// New thread
				let relations = BundledThread {
					latest_event: pdu.to_message_like_event(),
					count: uint!(1),
					current_user_participated: true,
				};

				let content = serde_json::to_value(relations).expect("to_value always works");

				unsigned.insert(
					"m.relations".to_owned(),
					json!({ "m.thread": content })
						.try_into()
						.expect("thread is valid json"),
				);
			}

			self.services
				.timeline
				.replace_pdu(&root_id, &root_pdu_json, &root_pdu)
				.await?;
		}

		let mut users = Vec::new();
		match self.get_participants(&root_id).await {
			| Ok(userids) => {
				users.extend_from_slice(&userids);
			},
			| _ => {
				users.push(root_pdu.sender);
			},
		}
		users.push(pdu.sender.clone());

		self.update_participants(&root_id, &users)
	}

	pub async fn threads_until<'a>(
		&'a self,
		user_id: &'a UserId,
		room_id: &'a RoomId,
		shorteventid: PduCount,
		_inc: &'a IncludeThreads,
	) -> Result<impl Stream<Item = (PduCount, PduEvent)> + Send + 'a> {
		let shortroomid: ShortRoomId = self
			.services
			.short
			.get_shortroomid(room_id)
			.await?;

		let current: RawPduId = PduId {
			shortroomid,
			shorteventid: shorteventid.saturating_sub(1),
		}
		.into();

		let stream = self
			.db
			.threadid_userids
			.rev_raw_keys_from(&current)
			.ignore_err()
			.map(RawPduId::from)
			.ready_take_while(move |pdu_id| pdu_id.shortroomid() == shortroomid.to_be_bytes())
			.wide_filter_map(move |pdu_id| async move {
				let mut pdu = self
					.services
					.timeline
					.get_pdu_from_id(&pdu_id)
					.await
					.ok()?;
				let pdu_id: PduId = pdu_id.into();

				if pdu.sender != user_id {
					pdu.remove_transaction_id().ok();
				}

				Some((pdu_id.shorteventid, pdu))
			});

		Ok(stream)
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
}
