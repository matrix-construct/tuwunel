use std::{mem::size_of, sync::Arc};

use futures::{Stream, StreamExt};
use ruma::{EventId, RoomId, UserId, api::Direction};
use tuwunel_core::{
	Result,
	arrayvec::ArrayVec,
	matrix::{Event, PduCount},
	result::LogErr,
	trace,
	utils::{
		ReadyExt,
		stream::{TryIgnore, WidebandExt},
		u64_from_u8,
	},
};
use tuwunel_database::{Interfix, Map};

use crate::rooms::{
	short::ShortRoomId,
	timeline::{PduId, RawPduId},
};

pub(super) struct Data {
	tofrom_relation: Arc<Map>,
	referencedevents: Arc<Map>,
	softfailedeventids: Arc<Map>,
	services: Arc<crate::services::OnceServices>,
}

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = &args.db;
		Self {
			tofrom_relation: db["tofrom_relation"].clone(),
			referencedevents: db["referencedevents"].clone(),
			softfailedeventids: db["softfailedeventids"].clone(),
			services: args.services.clone(),
		}
	}

	#[inline]
	pub(super) fn add_relation(&self, from: u64, to: u64) {
		const BUFSIZE: usize = size_of::<u64>() * 2;

		let key: &[u64] = &[to, from];
		self.tofrom_relation
			.aput_raw::<BUFSIZE, _, _>(key, []);
	}

	pub(super) fn get_relations<'a>(
		&'a self,
		user_id: &'a UserId,
		shortroomid: ShortRoomId,
		target: PduCount,
		from: PduCount,
		dir: Direction,
	) -> impl Stream<Item = (PduCount, impl Event)> + Send + '_ {
		let mut current = ArrayVec::<u8, 16>::new();
		current.extend(target.into_unsigned().to_be_bytes());
		current.extend(
			from.saturating_inc(dir)
				.into_unsigned()
				.to_be_bytes(),
		);
		let current = current.as_slice();
		match dir {
			| Direction::Forward => self
				.tofrom_relation
				.raw_keys_from(current)
				.boxed(),
			| Direction::Backward => self
				.tofrom_relation
				.rev_raw_keys_from(current)
				.boxed(),
		}
		.ignore_err()
		.ready_take_while(move |key| key.starts_with(&target.into_unsigned().to_be_bytes()))
		.map(|to_from| u64_from_u8(&to_from[8..16]))
		.map(PduCount::from_unsigned)
		.map(move |count| (user_id, shortroomid, count))
		.wide_filter_map(async |(user_id, shortroomid, count)| {
			let pdu_id: RawPduId = PduId { shortroomid, count }.into();
			let mut pdu = self
				.services
				.timeline
				.get_pdu_from_id(&pdu_id)
				.await
				.ok()?;

			if pdu.sender() != user_id {
				pdu.as_mut_pdu()
					.remove_transaction_id()
					.log_err()
					.ok();
			}

			Some((count, pdu))
		})
	}

	#[inline]
	pub(super) fn mark_as_referenced<'a, I>(&self, room_id: &RoomId, event_ids: I)
	where
		I: Iterator<Item = &'a EventId>,
	{
		for prev in event_ids {
			let key = (room_id, prev);
			self.referencedevents.put_raw(key, []);
		}
	}

	#[inline]
	pub(super) async fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> bool {
		let key = (room_id, event_id);
		self.referencedevents.qry(&key).await.is_ok()
	}

	#[inline]
	pub(super) fn mark_event_soft_failed(&self, event_id: &EventId) {
		self.softfailedeventids.insert(event_id, []);
	}

	#[inline]
	pub(super) async fn is_event_soft_failed(&self, event_id: &EventId) -> bool {
		self.softfailedeventids
			.get(event_id)
			.await
			.is_ok()
	}

	#[inline]
	pub(super) async fn delete_all_referenced_for_room(&self, room_id: &RoomId) -> Result {
		let prefix = (room_id, Interfix);

		self.referencedevents
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| {
				trace!("Removing key: {key:?}");
				self.referencedevents.remove(key);
			})
			.await;

		Ok(())
	}
}
