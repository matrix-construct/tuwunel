mod data;
use std::sync::Arc;

use futures::{StreamExt, future::try_join};
use ruma::{EventId, RoomId, UserId, api::Direction};
use tuwunel_core::{PduCount, Result};

use self::data::{Data, PdusIterItem};
use crate::{Dep, rooms};

pub struct Service {
	services: Services,
	db: Data,
}

struct Services {
	short: Dep<rooms::short::Service>,
	timeline: Dep<rooms::timeline::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: Services {
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
	#[tracing::instrument(skip(self, from, to), level = "debug")]
	pub fn add_relation(&self, from: PduCount, to: PduCount) {
		match (from, to) {
			| (PduCount::Normal(f), PduCount::Normal(t)) => self.db.add_relation(f, t),
			| _ => {
				// TODO: Relations with backfilled pdus
			},
		}
	}

	#[allow(clippy::too_many_arguments)]
	pub async fn get_relations(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		target: &EventId,
		from: PduCount,
		limit: usize,
		max_depth: u8,
		dir: Direction,
	) -> Vec<PdusIterItem> {
		let room_id = self.services.short.get_shortroomid(room_id);

		let target = self.services.timeline.get_pdu_count(target);

		let Ok((room_id, target)) = try_join(room_id, target).await else {
			return Vec::new();
		};

		let target = match target {
			| PduCount::Normal(c) => c,
			// TODO: Support backfilled relations
			| _ => 0, // This will result in an empty iterator
		};

		let mut pdus: Vec<_> = self
			.db
			.get_relations(user_id, room_id, target, from, dir)
			.collect()
			.await;

		let mut stack: Vec<_> = pdus
			.iter()
			.filter(|_| max_depth > 0)
			.map(|pdu| (pdu.clone(), 1))
			.collect();

		'limit: while let Some(stack_pdu) = stack.pop() {
			let target = match stack_pdu.0.0 {
				| PduCount::Normal(c) => c,
				// TODO: Support backfilled relations
				| PduCount::Backfilled(_) => 0, // This will result in an empty iterator
			};

			let relations: Vec<_> = self
				.db
				.get_relations(user_id, room_id, target, from, dir)
				.collect()
				.await;

			for relation in relations {
				if stack_pdu.1 < max_depth {
					stack.push((relation.clone(), stack_pdu.1.saturating_add(1)));
				}

				pdus.push(relation);
				if pdus.len() >= limit {
					break 'limit;
				}
			}
		}

		pdus
	}

	#[tracing::instrument(skip_all, level = "debug")]
	pub fn mark_as_referenced<'a, I>(&self, room_id: &RoomId, event_ids: I)
	where
		I: Iterator<Item = &'a EventId>,
	{
		self.db.mark_as_referenced(room_id, event_ids);
	}

	#[inline]
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> bool {
		self.db
			.is_event_referenced(room_id, event_id)
			.await
	}

	#[inline]
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn mark_event_soft_failed(&self, event_id: &EventId) {
		self.db.mark_event_soft_failed(event_id);
	}

	#[inline]
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn is_event_soft_failed(&self, event_id: &EventId) -> bool {
		self.db.is_event_soft_failed(event_id).await
	}
}
