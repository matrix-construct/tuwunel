use std::sync::Arc;

use futures::{Stream, StreamExt, TryFutureExt, future::Either};
use ruma::{EventId, RoomId, UserId, api::Direction};
use tuwunel_core::{
	PduId, Result,
	arrayvec::ArrayVec,
	implement,
	matrix::{Event, Pdu, PduCount, RawPduId},
	result::LogErr,
	trace,
	utils::{
		stream::{ReadyExt, TryIgnore, WidebandExt},
		u64_from_u8,
	},
};
use tuwunel_database::{Interfix, Map};

use crate::rooms::short::ShortRoomId;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	tofrom_relation: Arc<Map>,
	referencedevents: Arc<Map>,
	softfailedeventids: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data {
				tofrom_relation: args.db["tofrom_relation"].clone(),
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
