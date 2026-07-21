use futures::{Stream, StreamExt, TryFutureExt, future::Either};
use ruma::{
	EventId, OwnedUserId, UserId,
	api::Direction,
	events::{reaction::ReactionEventContent, relation::RelationType},
};
use tuwunel_core::{
	PduId,
	arrayvec::ArrayVec,
	implement, is_equal_to,
	matrix::{Event, Pdu, PduCount, RawPduId, event::RelationTypeEqual},
	result::LogErr,
	utils::{
		stream::{ReadyExt, TryIgnore, WidebandExt},
		u64_from_u8,
	},
};

use super::Service;
use crate::rooms::short::ShortRoomId;

type StartKey = ArrayVec<u8, 16>;

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

			// When key is supplied we don't need to double-parse the content here and
			// below.
			key.is_some() || rel_type.is_none_or(|rel_type| rel_type.relation_type_equal(&pdu))
		})
		.ready_filter(|(_, pdu)| {
			key.is_none_or(|key| {
				pdu.get_content()
					.map(|content: ReactionEventContent| content.relates_to.key == key)
					.unwrap_or(false)
			})
		})
		.ready_any(|_| true)
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
			let sender_matches =
				senders.is_empty() || senders.iter().any(is_equal_to!(pdu.sender()));

			let rel_type_matches = rel_types.is_empty()
				|| rel_types
					.iter()
					.any(|rel_type| rel_type.relation_type_equal(&pdu));

			sender_matches && rel_type_matches
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

	let mut buf = StartKey::new();
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
