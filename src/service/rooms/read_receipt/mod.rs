mod data;

use std::{collections::BTreeMap, sync::Arc};

use futures::{Stream, TryFutureExt, try_join};
use ruma::{
	OwnedEventId, OwnedUserId, RoomId, UserId,
	events::{
		AnySyncEphemeralRoomEvent, SyncEphemeralRoomEvent,
		receipt::{ReceiptEvent, ReceiptEventContent, Receipts},
	},
	serde::Raw,
};
use tuwunel_core::{
	Result, debug, err,
	matrix::pdu::{PduCount, PduId, RawPduId},
	warn,
};

use self::data::{Data, ReceiptItem};
use crate::{Dep, rooms, sending};

pub struct Service {
	services: Services,
	db: Data,
}

struct Services {
	sending: Dep<sending::Service>,
	short: Dep<rooms::short::Service>,
	timeline: Dep<rooms::timeline::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: Services {
				sending: args.depend::<sending::Service>("sending"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
			},
			db: Data::new(&args),
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

impl Service {
	/// Replaces the previous read receipt.
	pub async fn readreceipt_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		event: &ReceiptEvent,
	) {
		self.db
			.readreceipt_update(user_id, room_id, event)
			.await;
		self.services
			.sending
			.flush_room(room_id)
			.await
			.expect("room flush failed");
	}

	/// Gets the latest private read receipt from the user in the room
	pub async fn private_read_get(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<Raw<AnySyncEphemeralRoomEvent>> {
		let pdu_count = self
			.private_read_get_count(room_id, user_id)
			.map_err(|e| {
				err!(Database(warn!("No private read receipt was set in {room_id}: {e}")))
			});
		let shortroomid = self
			.services
			.short
			.get_shortroomid(room_id)
			.map_err(|e| {
				err!(Database(warn!(
					"Short room ID does not exist in database for {room_id}: {e}"
				)))
			});
		let (pdu_count, shortroomid) = try_join!(pdu_count, shortroomid)?;

		let shorteventid = PduCount::Normal(pdu_count);
		let pdu_id: RawPduId = PduId { shortroomid, shorteventid }.into();

		let pdu = self
			.services
			.timeline
			.get_pdu_from_id(&pdu_id)
			.await?;

		let event_id: OwnedEventId = pdu.event_id;
		let user_id: OwnedUserId = user_id.to_owned();
		let content: BTreeMap<OwnedEventId, Receipts> = BTreeMap::from_iter([(
			event_id,
			BTreeMap::from_iter([(
				ruma::events::receipt::ReceiptType::ReadPrivate,
				BTreeMap::from_iter([(
					user_id,
					ruma::events::receipt::Receipt {
						ts: None, // TODO: start storing the timestamp so we can return one
						thread: ruma::events::receipt::ReceiptThread::Unthreaded,
					},
				)]),
			)]),
		)]);
		let receipt_event_content = ReceiptEventContent(content);
		let receipt_sync_event = SyncEphemeralRoomEvent { content: receipt_event_content };

		let event = serde_json::value::to_raw_value(&receipt_sync_event)
			.expect("receipt created manually");

		Ok(Raw::from_json(event))
	}

	/// Returns an iterator over the most recent read_receipts in a room that
	/// happened after the event with id `since`.
	#[inline]
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn readreceipts_since<'a>(
		&'a self,
		room_id: &'a RoomId,
		since: u64,
	) -> impl Stream<Item = ReceiptItem<'_>> + Send + 'a {
		self.db.readreceipts_since(room_id, since)
	}

	/// Sets a private read marker at PDU `count`.
	#[inline]
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn private_read_set(&self, room_id: &RoomId, user_id: &UserId, count: u64) {
		self.db.private_read_set(room_id, user_id, count);
	}

	/// Returns the private read marker PDU count.
	#[inline]
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn private_read_get_count(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<u64> {
		self.db
			.private_read_get_count(room_id, user_id)
			.await
	}

	/// Returns the PDU count of the last typing update in this room.
	#[inline]
	pub async fn last_privateread_update(&self, user_id: &UserId, room_id: &RoomId) -> u64 {
		self.db
			.last_privateread_update(user_id, room_id)
			.await
	}
}

#[must_use]
pub fn pack_receipts<I>(receipts: I) -> Raw<SyncEphemeralRoomEvent<ReceiptEventContent>>
where
	I: Iterator<Item = Raw<AnySyncEphemeralRoomEvent>>,
{
	let mut json = BTreeMap::new();
	for value in receipts {
		let receipt = serde_json::from_str::<SyncEphemeralRoomEvent<ReceiptEventContent>>(
			value.json().get(),
		);
		match receipt {
			| Ok(value) => {
				for (event, receipt) in value.content {
					json.insert(event, receipt);
				}
			},
			| _ => {
				debug!("failed to parse receipt: {:?}", receipt);
			},
		}
	}
	let content = ReceiptEventContent::from_iter(json);

	tuwunel_core::trace!(?content);
	Raw::from_json(
		serde_json::value::to_raw_value(&SyncEphemeralRoomEvent { content })
			.expect("received valid json"),
	)
}
