use std::{
	collections::{BTreeMap, HashSet},
	sync::Arc,
};

use futures::StreamExt;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedUserId, RoomId, RoomVersionId, UserId,
	events::{
		GlobalAccountDataEventType, TimelineEventType,
		push_rules::PushRulesEvent,
		room::{
			encrypted::Relation,
			member::{MembershipState, RoomMemberEventContent},
			redaction::RoomRedactionEventContent,
		},
	},
	push::{Action, Ruleset, Tweak},
};
use tuwunel_core::{
	Result, err, error, implement,
	matrix::{
		event::Event,
		pdu::{PduCount, PduEvent, PduId, RawPduId},
	},
	utils::{self, ReadyExt, result::LogErr},
};
use tuwunel_database::{Json, Map};

use super::{ExtractBody, ExtractRelatesTo, ExtractRelatesToEventId, RoomMutexGuard};
use crate::rooms::state_compressor::CompressedState;

/// Append the incoming event setting the state snapshot to the state from
/// the server that sent the event.
#[implement(super::Service)]
#[tracing::instrument(
	name = "append_incoming",
	level = "debug",
	skip_all,
	ret(Debug)
)]
pub async fn append_incoming_pdu<'a, Leafs>(
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

		self.services
			.state
			.set_forward_extremities(&pdu.room_id, new_room_leafs, state_lock)
			.await;

		return Ok(None);
	}

	let pdu_id = self
		.append_pdu(pdu, pdu_json, new_room_leafs, state_lock)
		.await?;

	Ok(Some(pdu_id))
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
			.entry("unsigned".to_owned())
			.or_insert_with(|| CanonicalJsonValue::Object(BTreeMap::default()))
		{
			if let Ok(shortstatehash) = self
				.services
				.state
				.pdu_shortstatehash(pdu.event_id())
				.await
			{
				if let Ok(prev_state) = self
					.services
					.state_accessor
					.state_get(shortstatehash, &pdu.kind().to_string().into(), state_key)
					.await
				{
					unsigned.insert(
						"prev_content".to_owned(),
						CanonicalJsonValue::Object(
							utils::to_canonical_object(prev_state.get_content_as_value())
								.map_err(|e| {
									err!(Database(error!(
										"Failed to convert prev_state to canonical JSON: {e}",
									)))
								})?,
						),
					);
					unsigned.insert(
						String::from("prev_sender"),
						CanonicalJsonValue::String(prev_state.sender().to_string()),
					);
					unsigned.insert(
						String::from("replaces_state"),
						CanonicalJsonValue::String(prev_state.event_id().to_string()),
					);
				}
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
	// appending fails
	self.services
		.read_receipt
		.private_read_set(pdu.room_id(), pdu.sender(), *next_count2);

	self.services
		.user
		.reset_notification_counts(pdu.sender(), pdu.room_id());

	let count = PduCount::Normal(*next_count1);
	let pdu_id: RawPduId = PduId { shortroomid, count }.into();

	// Insert pdu
	self.append_pdu_json(&pdu_id, pdu, &pdu_json, count);

	drop(insert_lock);

	// Don't notify the sender of their own events, and dont send from ignored users
	let mut push_target: HashSet<_> = self
		.services
		.state_cache
		.active_local_users_in_room(pdu.room_id())
		.map(ToOwned::to_owned)
		.ready_filter(|user| *user != pdu.sender())
		.filter_map(async |recipient_user| {
			self.services
				.users
				.user_is_ignored(pdu.sender(), &recipient_user)
				.await
				.eq(&false)
				.then_some(recipient_user)
		})
		.collect()
		.await;

	let mut notifies = Vec::with_capacity(push_target.len().saturating_add(1));
	let mut highlights = Vec::with_capacity(push_target.len().saturating_add(1));

	if *pdu.kind() == TimelineEventType::RoomMember {
		if let Some(state_key) = pdu.state_key() {
			let target_user_id = UserId::parse(state_key)?;

			if self
				.services
				.users
				.is_active_local(target_user_id)
				.await
			{
				push_target.insert(target_user_id.to_owned());
			}
		}
	}

	let serialized = pdu.to_format();
	for user in &push_target {
		let rules_for_user = self
			.services
			.account_data
			.get_global(user, GlobalAccountDataEventType::PushRules)
			.await
			.map_or_else(
				|_| Ruleset::server_default(user),
				|ev: PushRulesEvent| ev.content.global,
			);

		let mut highlight = false;
		let mut notify = false;

		let power_levels = self
			.services
			.state_accessor
			.get_power_levels(pdu.room_id())
			.await?;

		for action in self
			.services
			.pusher
			.get_actions(user, &rules_for_user, &power_levels, &serialized, pdu.room_id())
			.await
		{
			match action {
				| Action::Notify => notify = true,
				| Action::SetTweak(Tweak::Highlight(true)) => {
					highlight = true;
				},
				| _ => {},
			}

			// Break early if both conditions are true
			if notify && highlight {
				break;
			}
		}

		if notify {
			notifies.push(user.clone());
		}

		if highlight {
			highlights.push(user.clone());
		}

		self.services
			.pusher
			.get_pushkeys(user)
			.ready_for_each(|push_key| {
				self.services
					.sending
					.send_pdu_push(&pdu_id, user, push_key.to_owned())
					.expect("TODO: replace with future");
			})
			.await;
	}

	self.increment_notification_counts(pdu.room_id(), notifies, highlights);

	match *pdu.kind() {
		| TimelineEventType::RoomRedaction => {
			use RoomVersionId::*;

			let room_version_id = self
				.services
				.state
				.get_room_version(pdu.room_id())
				.await?;

			match room_version_id {
				| V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 => {
					if let Some(redact_id) = pdu.redacts() {
						if self
							.services
							.state_accessor
							.user_can_redact(redact_id, pdu.sender(), pdu.room_id(), false)
							.await?
						{
							self.redact_pdu(redact_id, pdu, shortroomid)
								.await?;
						}
					}
				},
				| _ => {
					let content: RoomRedactionEventContent = pdu.get_content()?;
					if let Some(redact_id) = &content.redacts {
						if self
							.services
							.state_accessor
							.user_can_redact(redact_id, pdu.sender(), pdu.room_id(), false)
							.await?
						{
							self.redact_pdu(redact_id, pdu, shortroomid)
								.await?;
						}
					}
				},
			}
		},
		| TimelineEventType::SpaceChild =>
			if let Some(_state_key) = pdu.state_key() {
				self.services
					.spaces
					.roomid_spacehierarchy_cache
					.lock()
					.await
					.remove(pdu.room_id());
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
						target_user_id,
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
		| _ => {},
	}

	if let Ok(content) = pdu.get_content::<ExtractRelatesToEventId>() {
		if let Ok(related_pducount) = self
			.get_pdu_count(&content.relates_to.event_id)
			.await
		{
			self.services
				.pdu_metadata
				.add_relation(count, related_pducount);
		}
	}

	if let Ok(content) = pdu.get_content::<ExtractRelatesTo>() {
		match content.relates_to {
			| Relation::Reply { in_reply_to } => {
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
					.add_to_thread(&thread.event_id, pdu)
					.await?;
			},
			| _ => {}, // TODO: Aggregate other types
		}
	}

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
fn append_pdu_json(
	&self,
	pdu_id: &RawPduId,
	pdu: &PduEvent,
	json: &CanonicalJsonObject,
	count: PduCount,
) {
	debug_assert!(matches!(count, PduCount::Normal(_)), "PduCount not Normal");

	self.db.pduid_pdu.raw_put(pdu_id, Json(json));

	self.db
		.eventid_pduid
		.insert(pdu.event_id.as_bytes(), pdu_id);

	self.db
		.eventid_outlierpdu
		.remove(pdu.event_id.as_bytes());
}

#[implement(super::Service)]
fn increment_notification_counts(
	&self,
	room_id: &RoomId,
	notifies: Vec<OwnedUserId>,
	highlights: Vec<OwnedUserId>,
) {
	let _cork = self.db.db.cork();

	for user in notifies {
		let mut userroom_id = user.as_bytes().to_vec();
		userroom_id.push(0xFF);
		userroom_id.extend_from_slice(room_id.as_bytes());
		increment(&self.db.userroomid_notificationcount, &userroom_id);
	}

	for user in highlights {
		let mut userroom_id = user.as_bytes().to_vec();
		userroom_id.push(0xFF);
		userroom_id.extend_from_slice(room_id.as_bytes());
		increment(&self.db.userroomid_highlightcount, &userroom_id);
	}
}

//TODO: this is an ABA
fn increment(db: &Arc<Map>, key: &[u8]) {
	let old = db.get_blocking(key);
	let new = utils::increment(old.ok().as_deref());
	db.insert(key, new);
}
