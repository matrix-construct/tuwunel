//! Appends authenticated events to room timelines and applies their side effects.
//!
//! Incoming events first receive a state snapshot, while accepted events are
//! assigned a normal stream count and committed to the timeline. Subsequent
//! cache, indexing, notification, and membership effects are coordinated here.

use std::{collections::btree_map::Entry, sync::Arc};

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, MxcUri, UserId,
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
	Result, debug, debug_warn, err, error, implement,
	smallvec::SmallVec,
	utils::{
		OptionExt,
		result::{LogErr, NotFound},
	},
	warn,
};
use tuwunel_database::Json;
use tuwunel_matrix::{
	event::Event,
	pdu::{PduCount, PduEvent, PduId, RawPduId},
	room_version,
};

use super::{ExtractBody, ExtractRelatesTo, ExtractRelatesToEventId, RoomMutexGuard, bias_count};
use crate::{
	Services,
	admin::CommandInput,
	rooms::{
		read_receipt::PrivateRead, short::ShortRoomId, state_accessor::plain_text_topic,
		state_cache::MembershipUpdate, state_compressor::CompressedState, timeline::ExtractUrl,
	},
};

type Band<'a> = SmallVec<[&'a EventId; 1]>;

struct Appended<'a> {
	pdu_id: RawPduId,
	pdu: &'a PduEvent,
	shortroomid: ShortRoomId,
	count: PduCount,

	/// Stream position of the same user's join a member event replaced, if any.
	replaced_join: Option<PduCount>,
}

/// Appends an incoming event with its locally resolved state snapshot.
///
/// The snapshot is recorded even when the event is soft-failed. A soft-failed
/// event is not inserted into the accepted timeline, but its predecessors are
/// marked referenced. Only a nonempty replacement extremity band is stored; an
/// empty calculation preserves the prior band.
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

/// Persists an authenticated event and applies its timeline side effects.
///
/// This method performs no authentication. The accepted row, event mapping,
/// outlier removal, and timestamp index are committed together, but later
/// cache, indexing, notification, and membership work is not part of that
/// transaction, so an error can be returned after the event is stored.
#[implement(super::Service)]
#[tracing::instrument(name = "append", level = "debug", skip_all, ret(Debug))]
pub async fn append_pdu<'a, Leafs>(
	&'a self,
	pdu: &'a PduEvent,
	pdu_json: CanonicalJsonObject,
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

	let (pdu_json, replaced_join) = self.read_replaced(pdu, pdu_json).await?;

	// We must keep track of all events that have been referenced.
	self.services
		.pdu_metadata
		.mark_as_referenced(pdu.room_id(), pdu.prev_events().map(AsRef::as_ref));

	self.services
		.state
		.set_forward_extremities(pdu.room_id(), leafs, state_lock)
		.await;

	let insert_lock = self.mutex_insert.lock(pdu.room_id()).await;
	let next_count = self.services.globals.next_count();

	// Mark as read first so the sending client doesn't get a notification even if
	// appending fails. Route through the dispatcher so per-thread counts are
	// also cleared; the sender's own send subsumes any thread receipt.
	self.services
		.read_receipt
		.private_read_set(PrivateRead {
			room_id: pdu.room_id(),
			user_id: pdu.sender(),
			count: *next_count,
			ts: pdu.origin_server_ts(),
			thread: &ReceiptThread::Unthreaded,
			announce: false,
		})
		.await;

	self.services
		.pusher
		.reset_notification_counts_for_thread(
			pdu.sender(),
			pdu.room_id(),
			None,
			&ReceiptThread::Unthreaded,
		)
		.await;

	let count = PduCount::Normal(*next_count);
	let pdu_id: RawPduId = PduId { shortroomid, count }.into();

	// Insert pdu
	self.append_pdu_json(&pdu_id, pdu, &pdu_json);

	drop(insert_lock);

	// Only local senders can own pushers.
	if self.services.globals.user_is_local(pdu.sender()) {
		self.services
			.sending
			.refresh_push_badge(pdu.sender())
			.await
			.log_err()
			.ok();
	}

	self.services
		.pusher
		.append_pdu(pdu_id, pdu)
		.await
		.log_err()
		.ok();

	let appended = Appended {
		pdu_id,
		pdu,
		shortroomid,
		count,
		replaced_join,
	};

	self.append_pdu_effects(appended, state_lock)
		.await?;

	drop(next_count);

	self.services
		.appservice
		.append_pdu(pdu_id, pdu)
		.await
		.log_err()
		.ok();

	Ok(pdu_id)
}

/// Fill a state event's `unsigned` from the event it replaced, and find the
/// position of the same user's join that a profile change replaces.
///
/// The replaced event is read once from the event's own state snapshot and is
/// dropped on return, so it holds no place across the append's later awaits.
#[implement(super::Service)]
async fn read_replaced(
	&self,
	pdu: &PduEvent,
	pdu_json: CanonicalJsonObject,
) -> Result<(CanonicalJsonObject, Option<PduCount>)> {
	let state_key = pdu.state_key();
	let prev_state = state_key
		.map_async(|state_key| self.prev_state(pdu, state_key))
		.await
		.flatten();

	let pdu_json = if state_key.is_some() {
		with_prev_unsigned(pdu_json, prev_state.as_ref())?
	} else {
		pdu_json
	};

	let replaced_join = self.replaced_join(pdu, prev_state.as_ref()).await;

	Ok((pdu_json, replaced_join))
}

#[implement(super::Service)]
async fn prev_state(&self, pdu: &PduEvent, state_key: &str) -> Option<PduEvent> {
	let event_id = pdu.event_id();
	let shortstatehash = self
		.services
		.state
		.pdu_shortstatehash(event_id)
		.await
		.optional()
		.inspect_err(|error| debug_warn!(%event_id, %error, "State snapshot read failed."))
		.ok()
		.flatten()?;

	let event_type = pdu.kind().to_cow_str().into();

	self.services
		.state_accessor
		.state_get(shortstatehash, &event_type, state_key)
		.await
		.optional()
		.inspect_err(|error| debug_warn!(%event_id, %error, "Replaced state read failed."))
		.ok()
		.flatten()
}

/// Fill a state event's `unsigned` from the event it replaced.
///
/// Adds the replaced event's `prev_content`, `prev_sender` and
/// `replaces_state`, from which clients read a membership or other state
/// change. A non-object `unsigned` is logged and left as it is.
fn with_prev_unsigned(
	mut pdu_json: CanonicalJsonObject,
	prev_state: Option<&PduEvent>,
) -> Result<CanonicalJsonObject> {
	let replaced = || {
		prev_state
			.map(prev_state_unsigned)
			.transpose()
			.map(Option::unwrap_or_default)
	};

	match pdu_json.entry("unsigned".into()) {
		| Entry::Vacant(entry) => {
			entry.insert(CanonicalJsonValue::Object(replaced()?));
		},
		| Entry::Occupied(entry) => match entry.into_mut() {
			| CanonicalJsonValue::Object(unsigned) => unsigned.extend(replaced()?),
			| _ => error!("Invalid unsigned type in pdu."),
		},
	}

	Ok(pdu_json)
}

fn prev_state_unsigned(prev_state: &PduEvent) -> Result<CanonicalJsonObject> {
	let prev_content = prev_state
		.get_content::<CanonicalJsonObject>()
		.map_err(|e| {
			err!(Database(error!("Failed to convert prev_state to canonical JSON: {e}")))
		})?;

	let unsigned = [
		("prev_content".into(), CanonicalJsonValue::Object(prev_content)),
		(
			"prev_sender".into(),
			CanonicalJsonValue::String(prev_state.sender().to_string()),
		),
		(
			"replaces_state".into(),
			CanonicalJsonValue::String(prev_state.event_id().to_string()),
		),
	]
	.into();

	Ok(unsigned)
}

/// Stream position of the same user's join that a member event replaces, when
/// the event is itself a join, as a profile change is.
///
/// `prev_state` must come from the event's own state snapshot, never the state
/// cache, which a forced replay may have moved ahead of this append. Any other
/// event yields `None`, as does a replaced join without a readable position,
/// which a failed read logs.
#[implement(super::Service)]
async fn replaced_join(&self, pdu: &PduEvent, prev_state: Option<&PduEvent>) -> Option<PduCount> {
	let user_id: &UserId = pdu.state_key()?.try_into().ok()?;
	let is_join = |event: &PduEvent| event.membership_for(user_id) == Some(MembershipState::Join);
	let replaced = prev_state.filter(|prev| is_join(prev) && is_join(pdu))?;
	let event_id = replaced.event_id();

	self.get_pdu_count(event_id)
		.await
		.optional()
		.inspect_err(
			|error| debug_warn!(%event_id, %error, "Replaced join position read failed."),
		)
		.ok()
		.flatten()
}

#[implement(super::Service)]
async fn append_pdu_effects(
	&self,
	Appended {
		pdu_id,
		pdu,
		shortroomid,
		count,
		replaced_join,
	}: Appended<'_>,
	state_lock: &RoomMutexGuard,
) -> Result {
	match *pdu.kind() {
		| TimelineEventType::RoomMember =>
			self.append_member_effects(pdu, replaced_join, count)
				.await?,
		| TimelineEventType::RoomTopic =>
			if let Some(topic) = pdu.get_content().ok().and_then(plain_text_topic) {
				self.services
					.search
					.index_pdu(shortroomid, &pdu_id, &topic);
			},
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
		| TimelineEventType::RoomMessage => {
			self.append_message_effects(&pdu_id, pdu, shortroomid)
				.await?;

			if self.services.config.media.auto_download_media {
				let content: ExtractUrl = pdu.get_content()?;
				if let Some(url) = content.url {
					self.services.server.runtime().spawn({
						let services = self.services.clone();
						async move {
							debug!(%url, "Auto downloading media");

							if let Err(err) = auto_download_media(&services, &url).await {
								warn!(%url, "Error auto downloading media: {err}");
							}
						}
					});
				}
			}
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

async fn auto_download_media(services: &Services, mxc_uri: &MxcUri) -> Result {
	let mxc = mxc_uri.parts()?;

	services
		.media
		.get_or_fetch(&mxc, ruma::media::default_download_timeout())
		.await?;

	Ok(())
}

/// Record the membership transition an `m.room.member` event carries.
///
/// The cache is written here rather than off the resolved state so that a
/// user who is invited or knocked and leaves immediately still leaves the
/// earlier event on record for auth. A join that replaced the same user's join
/// is a profile change, which keeps the user's recorded join position.
#[implement(super::Service)]
async fn append_member_effects(
	&self,
	pdu: &PduEvent,
	replaced_join: Option<PduCount>,
	count: PduCount,
) -> Result {
	let Some(state_key) = pdu.state_key() else {
		return Ok(());
	};

	let user_id = UserId::parse(state_key).expect("This state_key was previously validated");
	let content: RoomMemberEventContent = pdu.get_content()?;
	let is_invite = content.membership == MembershipState::Invite;
	let is_direct = content.is_direct;

	let stripped_state = match content.membership {
		| MembershipState::Invite | MembershipState::Knock => self
			.services
			.state
			.summary_stripped(pdu)
			.await
			.into(),
		| _ => None,
	};

	self.services
		.state_cache
		.update_membership(MembershipUpdate {
			room_id: pdu.room_id(),
			user_id: &user_id,
			membership_event: content,
			sender: pdu.sender(),
			last_state: stripped_state,
			invite_via: None,
			update_joined_count: true,
			replaced_join,
			count,
		})
		.await?;

	if is_invite {
		self.services
			.membership
			.auto_accept(pdu.room_id(), &user_id, pdu.sender(), is_direct);
	}

	Ok(())
}

/// Index an `m.room.message` event's body, and queue it when it is an admin
/// command.
///
/// The queued command carries the event's sender, so a handler can tell who
/// issued it, and the event's id, which its response replies to.
#[implement(super::Service)]
async fn append_message_effects(
	&self,
	pdu_id: &RawPduId,
	pdu: &PduEvent,
	shortroomid: ShortRoomId,
) -> Result {
	let content: ExtractBody = pdu.get_content()?;
	let Some(body) = content.body else {
		return Ok(());
	};

	self.services
		.search
		.index_pdu(shortroomid, pdu_id, &body);

	if self
		.services
		.admin
		.is_admin_command(pdu, &body)
		.await
	{
		self.services
			.admin
			.command(CommandInput {
				command: body,
				reply_id: Some(pdu.event_id().into()),
				sender: Some(pdu.sender().into()),
			})
			.await?;
	}

	Ok(())
}

#[implement(super::Service)]
fn append_pdu_json(&self, pdu_id: &RawPduId, pdu: &PduEvent, json: &CanonicalJsonObject) {
	debug_assert!(matches!(pdu_id.pdu_count(), PduCount::Normal(_)), "PduCount not Normal");

	let mut txn = self.db.db.txn();

	txn.raw_put(&self.db.pduid_pdu, pdu_id, Json(json));
	txn.insert_raw(&self.db.eventid_pduid, pdu.event_id.as_bytes(), pdu_id);
	txn.del_raw(&self.db.eventid_outlierpdu, pdu.event_id.as_bytes());

	let count_key = bias_count(pdu_id.count());
	let ts = u64::from(pdu.origin_server_ts);
	let key = (pdu.room_id(), ts, count_key);
	txn.put_raw(&self.db.roomid_tscount_pducount, key, pdu_id.count());

	txn.execute();
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
