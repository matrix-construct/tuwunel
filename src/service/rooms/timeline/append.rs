use std::{collections::BTreeMap, sync::Arc};

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, UserId,
	events::{
		TimelineEventType,
		receipt::ReceiptThread,
		relation::RelationType,
		room::{
			encrypted::Relation,
			member::{MembershipState, RoomMemberEventContent},
		},
	},
};
use tuwunel_core::{
	Result, err, error, implement,
	matrix::{
		event::Event,
		pdu::{PduCount, PduEvent, PduId, RawPduId},
		room_version,
	},
	smallvec::SmallVec,
	utils::{self, result::LogErr},
};
use tuwunel_database::Json;

use super::{ExtractBody, ExtractRelatesTo, ExtractRelatesToEventId, RoomMutexGuard, bias_count};
use crate::rooms::{
	short::ShortRoomId, state_accessor::plain_text_topic, state_compressor::CompressedState,
};

type Band<'a> = SmallVec<[&'a EventId; 1]>;

/// Append the incoming event setting the state snapshot to the state from
/// the server that sent the event.
#[implement(super::Service)]
#[tracing::instrument(
	name = "append_incoming",
	level = "debug",
	skip_all,
	ret(Debug)
)]
pub(crate) async fn append_incoming_pdu<'a, Leafs>(
	&'a self,
	pdu: &'a PduEvent,
	pdu_json: CanonicalJsonObject,
	new_room_leafs: Leafs,
	state_ids_compressed: Arc<CompressedState>,
	soft_fail: bool,
	state_lock: &'a RoomMutexGuard,
) -> Result<Option<RawPduId>>
where
	Leafs: Iterator<Item = &'a EventId> + Send + 'a,
{
	// We append to state before appending the pdu, so we don't have a moment in
	// time with the pdu without it's state. This is okay because append_pdu can't
	// fail.
	self.services
		.state
		.set_event_state(&pdu.event_id, &pdu.room_id, state_ids_compressed)
		.await?;

	if soft_fail {
		self.services
			.pdu_metadata
			.mark_as_referenced(&pdu.room_id, pdu.prev_events.iter().map(AsRef::as_ref));

		// Keep the previous band rather than let a soft-failed event empty it; a
		// later accepted event self-chains and heals it.
		if let Some(new_room_leafs) = nonempty_band(new_room_leafs) {
			self.services
				.state
				.set_forward_extremities(&pdu.room_id, new_room_leafs.into_iter(), state_lock)
				.await;
		}

		return Ok(None);
	}

	let pdu_id = self
		.append_pdu(pdu, pdu_json, new_room_leafs, state_lock)
		.await?;

	Ok(Some(pdu_id))
}

fn nonempty_band<'a, Leafs>(leafs: Leafs) -> Option<Band<'a>>
where
	Leafs: Iterator<Item = &'a EventId>,
{
	let leafs: Band<'_> = leafs.collect();

	(!leafs.is_empty()).then_some(leafs)
}

/// Creates a new persisted data unit and adds it to a room.
///
/// By this point the incoming event should be fully authenticated, no auth
/// happens in `append_pdu`.
///
/// Returns pdu id
#[implement(super::Service)]
#[tracing::instrument(name = "append", level = "debug", skip_all, ret(Debug))]
pub async fn append_pdu<'a, Leafs>(
	&'a self,
	pdu: &'a PduEvent,
	mut pdu_json: CanonicalJsonObject,
	leafs: Leafs,
	state_lock: &'a RoomMutexGuard,
) -> Result<RawPduId>
where
	Leafs: Iterator<Item = &'a EventId> + Send + 'a,
{
	// Coalesce database writes for the remainder of this scope.
	let _cork = self.db.db.cork_and_flush();

	let shortroomid = self
		.services
		.short
		.get_shortroomid(pdu.room_id())
		.await
		.map_err(|_| err!(Database("Room does not exist")))?;

	// Make unsigned fields correct. This is not properly documented in the spec,
	// but state events need to have previous content in the unsigned field, so
	// clients can easily interpret things like membership changes
	if let Some(state_key) = pdu.state_key() {
		if let CanonicalJsonValue::Object(unsigned) = pdu_json
			.entry("unsigned".into())
			.or_insert_with(|| CanonicalJsonValue::Object(BTreeMap::default()))
		{
			if let Ok(shortstatehash) = self
				.services
				.state
				.pdu_shortstatehash(pdu.event_id())
				.await && let Ok(prev_state) = self
				.services
				.state_accessor
				.state_get(shortstatehash, &pdu.kind().to_string().into(), state_key)
				.await
			{
				unsigned.insert(
					"prev_content".into(),
					CanonicalJsonValue::Object(
						utils::to_canonical_object(prev_state.get_content_as_value()).map_err(
							|e| {
								err!(Database(error!(
									"Failed to convert prev_state to canonical JSON: {e}",
								)))
							},
						)?,
					),
				);
				unsigned.insert(
					"prev_sender".into(),
					CanonicalJsonValue::String(prev_state.sender().to_string()),
				);
				unsigned.insert(
					"replaces_state".into(),
					CanonicalJsonValue::String(prev_state.event_id().to_string()),
				);
			}
		} else {
			error!("Invalid unsigned type in pdu.");
		}
	}

	// We must keep track of all events that have been referenced.
	self.services
		.pdu_metadata
		.mark_as_referenced(pdu.room_id(), pdu.prev_events().map(AsRef::as_ref));

	self.services
		.state
		.set_forward_extremities(pdu.room_id(), leafs, state_lock)
		.await;

	let insert_lock = self.mutex_insert.lock(pdu.room_id()).await;
	let next_count1 = self.services.globals.next_count();
	let next_count2 = self.services.globals.next_count();

	// Mark as read first so the sending client doesn't get a notification even if
	// appending fails. Route through the dispatcher so per-thread counts are
	// also cleared; the sender's own send subsumes any thread receipt.
	self.services
		.read_receipt
		.private_read_set(
			pdu.room_id(),
			pdu.sender(),
			*next_count2,
			pdu.origin_server_ts(),
			&ReceiptThread::Unthreaded,
		)
		.await;

	self.services
		.pusher
		.reset_notification_counts_for_thread(
			pdu.sender(),
			pdu.room_id(),
			&ReceiptThread::Unthreaded,
		)
		.await;

	let count = PduCount::Normal(*next_count1);
	let pdu_id: RawPduId = PduId { shortroomid, count }.into();

	// Insert pdu
	self.append_pdu_json(&pdu_id, pdu, &pdu_json);

	drop(insert_lock);
	self.services
		.pusher
		.schedule_badge_update(pdu.sender().to_owned());

	self.services
		.pusher
		.append_pdu(pdu_id, pdu)
		.await
		.log_err()
		.ok();

	self.append_pdu_effects(pdu_id, pdu, shortroomid, count, state_lock)
		.await?;

	drop(next_count1);
	drop(next_count2);

	self.services
		.appservice
		.append_pdu(pdu_id, pdu)
		.await
		.log_err()
		.ok();

	Ok(pdu_id)
}

#[implement(super::Service)]
async fn append_pdu_effects(
	&self,
	pdu_id: RawPduId,
	pdu: &PduEvent,
	shortroomid: ShortRoomId,
	count: PduCount,
	state_lock: &RoomMutexGuard,
) -> Result {
	match *pdu.kind() {
		| TimelineEventType::RoomRedaction => {
			let room_version = self
				.services
				.state
				.get_room_version(pdu.room_id())
				.await?;

			let room_rules = room_version::rules(&room_version)?;

			let redacts_id = pdu.redacts_id(&room_rules);

			if let Some(redacts_id) = &redacts_id
				&& self
					.services
					.state_accessor
					.user_can_redact(redacts_id, pdu.sender(), pdu.room_id(), false)
					.await?
			{
				self.redact_pdu(redacts_id, pdu, shortroomid, state_lock)
					.await?;
			}
		},
		| TimelineEventType::RoomMember => {
			if let Some(state_key) = pdu.state_key() {
				// if the state_key fails
				let target_user_id =
					UserId::parse(state_key).expect("This state_key was previously validated");

				let content: RoomMemberEventContent = pdu.get_content()?;
				let stripped_state = match content.membership {
					| MembershipState::Invite | MembershipState::Knock => self
						.services
						.state
						.summary_stripped(pdu)
						.await
						.into(),
					| _ => None,
				};

				// Update our membership info, we do this here incase a user is invited or
				// knocked and immediately leaves we need the DB to record the invite or
				// knock event for auth
				self.services
					.state_cache
					.update_membership(
						pdu.room_id(),
						&target_user_id,
						content,
						pdu.sender(),
						stripped_state,
						None,
						true,
						count,
					)
					.await?;
			}
		},
		| TimelineEventType::RoomMessage => {
			let content: ExtractBody = pdu.get_content()?;
			if let Some(body) = content.body {
				self.services
					.search
					.index_pdu(shortroomid, &pdu_id, &body);

				if self
					.services
					.admin
					.is_admin_command(pdu, &body)
					.await
				{
					self.services
						.admin
						.command(body, Some((pdu.event_id()).into()))
						.await?;
				}
			}
		},
		| TimelineEventType::RoomTopic =>
			if let Some(topic) = pdu.get_content().ok().and_then(plain_text_topic) {
				self.services
					.search
					.index_pdu(shortroomid, &pdu_id, &topic);
			},
		| _ => {},
	}

	// The cached hierarchy summary projects room state; evict on any state change.
	if pdu.state_key().is_some() {
		self.services.spaces.cache_evict(pdu.room_id());
	}

	if let Ok(content) = pdu.get_content::<ExtractRelatesToEventId>()
		&& let Ok(related_pducount) = self
			.get_pdu_count(&content.relates_to.event_id)
			.await
	{
		self.services
			.pdu_metadata
			.add_relation(count, related_pducount);
	}

	if let Ok(content) = pdu.get_content::<ExtractRelatesTo>() {
		match content.relates_to {
			| Relation::Reply(ruma::events::relation::Reply { in_reply_to }) => {
				// We need to do it again here, because replies don't have
				// event_id as a top level field
				if let Ok(related_pducount) = self.get_pdu_count(&in_reply_to.event_id).await {
					self.services
						.pdu_metadata
						.add_relation(count, related_pducount);
				}
			},
			| Relation::Thread(thread) => {
				self.services
					.threads
					.add_to_thread(&thread.event_id, pdu_id, pdu)
					.await?;
			},
			| Relation::Replacement(replacement) => {
				self.services
					.pdu_metadata
					.add_typed_relation(
						shortroomid,
						count,
						&replacement.event_id,
						pdu,
						RelationType::Replacement,
					)
					.await;
			},
			| Relation::Reference(reference) => {
				self.services
					.pdu_metadata
					.add_typed_relation(
						shortroomid,
						count,
						&reference.event_id,
						pdu,
						RelationType::Reference,
					)
					.await;
			},
			| _ => {}, // TODO: Aggregate other types
		}
	}

	Ok(())
}

#[implement(super::Service)]
fn append_pdu_json(&self, pdu_id: &RawPduId, pdu: &PduEvent, json: &CanonicalJsonObject) {
	debug_assert!(matches!(pdu_id.pdu_count(), PduCount::Normal(_)), "PduCount not Normal");

	self.db.pduid_pdu.raw_put(pdu_id, Json(json));

	self.db
		.eventid_pduid
		.insert(pdu.event_id.as_bytes(), pdu_id);

	self.db
		.eventid_outlierpdu
		.remove(pdu.event_id.as_bytes());

	let ts = u64::from(pdu.origin_server_ts);
	let count_key = bias_count(pdu_id.count());

	self.db
		.roomid_tscount_pducount
		.put_raw((pdu.room_id(), ts, count_key), pdu_id.count());
}

#[cfg(test)]
mod tests {
	use std::iter::empty;

	use ruma::event_id;

	use super::*;

	#[test]
	fn empty_band_is_skipped() {
		assert!(nonempty_band(empty::<&EventId>()).is_none());
	}

	#[test]
	fn nonempty_band_preserves_all_leaves() {
		let leaves = [event_id!("$a:test.local"), event_id!("$b:test.local")];

		let kept: Vec<&EventId> = nonempty_band(leaves.iter().copied())
			.expect("non-empty band retained")
			.into_iter()
			.collect();

		assert_eq!(kept, leaves);
	}
}
