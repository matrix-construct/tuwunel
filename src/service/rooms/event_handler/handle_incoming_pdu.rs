use futures::{FutureExt, TryFutureExt, TryStreamExt, future::try_join5};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomId, ServerName, UserId,
	events::{
		AnyStrippedStateEvent, StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use tuwunel_core::{
	Err, Result, async_noinline, debug,
	debug::INFO_SPAN_LEVEL,
	debug_warn, err, implement,
	matrix::{Event, PduCount, PduEvent, pdu::MAX_PREV_EVENTS, room_version::from_create_event},
	smallvec::SmallVec,
	trace,
	utils::{
		BoolExt,
		future::ReadyEqExt,
		stream::{IterStream, TryBroadbandExt, TryReadyExt},
	},
	warn,
};

use super::{
	backoff::{Context, Disposition, Suppression, UPGRADE_RETRY},
	fetch_prev::{Pdus, PrevFetch},
	handle_prev_pdu::PrevUpgrade,
	prev_walk::PrevWalk,
	room_version_of,
};
use crate::rooms::{state_cache::MembershipUpdate, timeline::RawPduId};

type PrevSplit = SmallVec<[OwnedEventId; MAX_PREV_EVENTS]>;

type Handled = Option<(RawPduId, bool)>;

/// When receiving an event one needs to:
/// 0. Check the server is in the room
/// 1. Skip the PDU if we already know about it
/// 1.1. Remove unsigned field
/// 2. Check signatures, otherwise drop
/// 3. Check content hash, redact if doesn't match
/// 4. Fetch any missing auth events doing all checks listed here starting at 1.
///    These are not timeline events
/// 5. Reject "due to auth events" if can't get all the auth events or some of
///    the auth events are also rejected "due to auth events"
/// 6. Reject "due to auth events" if the event doesn't pass auth based on the
///    auth events
/// 7. Persist this event as an outlier
/// 8. If not timeline event: stop
/// 9. Fetch any missing prev events doing all checks listed here starting at 1.
///    These are timeline events
/// 10. Fetch missing state and auth chain events by calling `/state_ids` at
///     backwards extremities doing all the checks in this list starting at
///     1. These are not timeline events
/// 11. Check the auth of the event passes based on the state of the event
/// 12. Ensure that the state is derived from the previous current state (i.e.
///     we calculated by doing state res where one of the inputs was a
///     previously trusted set of state, don't just trust a set of state we got
///     from a remote)
/// 13. Use state resolution to find new room state
/// 14. Check if the event passes auth based on the "current state" of the room,
///     if not soft fail it
#[implement(super::Service)]
// cross-crate codegen firewall
#[async_noinline]
#[tracing::instrument(
	name = "pdu",
	level = INFO_SPAN_LEVEL,
	skip_all,
	fields(%room_id, %event_id),
	ret(level = "debug"),
)]
pub async fn handle_incoming_pdu<'a>(
	&'a self,
	origin: &'a ServerName,
	room_id: &'a RoomId,
	event_id: &'a EventId,
	pdu: CanonicalJsonObject,
	is_timeline_event: bool,
) -> Result<Handled> {
	// 1. Skip the PDU if we already have it as a timeline event
	if let Ok(pdu_id) = self.services.timeline.get_pdu_id(event_id).await {
		debug!(?pdu_id, "Exists.");
		return Ok(Some((pdu_id, false)));
	}

	// 1.1 Check the server is in the room
	let meta_exists = self.services.metadata.exists(room_id).map(Ok);

	// 1.2 Check if the room is disabled
	let is_disabled = self
		.services
		.metadata
		.is_disabled(room_id)
		.map(Ok);

	// 1.3.1 Check room ACL on origin field/server
	let origin_acl_check = self.acl_check(origin, room_id);

	// 1.3.2 Check room ACL on sender's server name
	let sender: &UserId = pdu
		.get("sender")
		.try_into()
		.map_err(|e| err!(Request(InvalidParam("PDU does not have a valid sender key: {e}"))))?;

	let sender_acl_check = sender
		.server_name()
		.ne(origin)
		.then_async(|| self.acl_check(sender.server_name(), room_id));

	// Fetch create event; absent when we are not resident in the room.
	let create_event = self
		.services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomCreate, "")
		.map(|result| Ok(result.ok()));

	let (meta_exists, is_disabled, (), (), create_event) = try_join5(
		meta_exists,
		is_disabled,
		origin_acl_check,
		sender_acl_check.map(|o| o.unwrap_or(Ok(()))),
		create_event,
	)
	.await?;

	// When not resident, the only event we can act on is a leave rescinding an
	// out-of-band invite we hold for a local user.
	if !meta_exists {
		return if self
			.handle_rescinded_invite(room_id, &pdu)
			.await?
		{
			Ok(None)
		} else {
			Err!(Request(NotFound("Room is unknown to this server")))
		};
	}

	if is_disabled {
		return Err!(Request(Forbidden("Federation of this room is disabled by this server.")));
	}

	let create_event =
		create_event.ok_or_else(|| err!(Request(NotFound("Room is unknown to this server"))))?;

	let room_version = from_create_event(&create_event)?;
	let recursion_level = 0;

	let (incoming_pdu, pdu) = self
		.handle_outlier_pdu(origin, room_id, event_id, pdu, &room_version, recursion_level, false)
		.await?;

	// 8. if not timeline event: stop
	if !is_timeline_event {
		debug!(
			kind = ?incoming_pdu.event_type(),
			"Not a timeline event.",
		);

		return Ok(None);
	}

	// Skip old events
	let first_normal_ts = self
		.services
		.timeline
		.first_normal_pdu_in_room(room_id)
		.await?
		.origin_server_ts();

	if incoming_pdu.origin_server_ts() < first_normal_ts {
		debug!(
			origin_server_ts = ?incoming_pdu.origin_server_ts(),
			?first_normal_ts,
			"Skipping old event."
		);
		return Ok(None);
	}

	let gapped = self
		.services
		.timeline
		.non_outlier_pdus_exist(incoming_pdu.prev_events())
		.await
		.is_false();

	self.prev_walk.enter(gapped);

	let create_event_id = create_event.event_id();
	let upgrade = PrevUpgrade {
		origin,
		room_id,
		event_id,
		room_version: &room_version,
		recursion_level,
		first_normal_ts,
		create_event_id,
	};

	// Start before the first await so a dropped future still settles the gapped count.
	let pass = gapped.then(|| PrevWalk::start(self, &upgrade));

	let standing = gapped
		.then_async(|| self.is_suppressed(Context::Incoming, event_id, UPGRADE_RETRY))
		.await
		.unwrap_or(Suppression::Absent);

	if standing.is_deny() {
		if let Some(pass) = pass {
			pass.hold();
		}

		debug_warn!(%origin, %room_id, %event_id, "Backing off from a gapped incoming event.");
		return Ok(None);
	}

	// 9. Fetch any missing prev events doing all checks listed here starting at 1.
	//    These are timeline events
	let fetch = self
		.fetch_prev(upgrade, incoming_pdu.prev_events())
		.await;

	let stopping = self.services.server.is_stopping();
	let walk = pass.and_then(|pass| pass.fetched(fetch.as_ref(), stopping));
	let fetch = fetch?;

	let walking = fetch.sorted.is_empty().is_false();
	let attempt = walking.then(|| self.record_attempt(Context::Incoming, event_id));
	let PrevFetch { sorted, pdus, .. } = fetch;

	let (handled, upgraded) = self
		.handle_prev_events(upgrade, sorted, pdus)
		.boxed() // size firewall
		.and_then(|upgraded| {
			self.upgrade_outlier_to_timeline_pdu(upgrade, incoming_pdu, pdu)
				.boxed() // size firewall
				.map(move |handled| Ok((handled, upgraded)))
		})
		.unwrap_or_else(|error| (Err(error), 0))
		.await;

	let appended = handled.as_ref().map(Option::is_some);

	if let Some(walk) = walk {
		walk.settle(appended, upgraded, self.services.server.is_stopping());
	}

	self.record_completion(Context::Incoming, event_id, standing, attempt, appended)
		.await;

	handled
}

/// Apply a federated leave that rescinds an out-of-band invite for a local
/// user.
///
/// We are not resident in the room, so the kick cannot be processed as a normal
/// timeline event for lack of room state; but it must still clear the invite so
/// the invited user's `/sync` reflects the rescission. Mirrors Synapse's
/// out-of-band membership handling: only a kick from the original inviter is
/// honored, since without the room state we cannot judge any other sender's
/// authority. Returns `true` when a rescission was applied.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "debug", fields(%room_id))]
async fn handle_rescinded_invite(
	&self,
	room_id: &RoomId,
	pdu: &CanonicalJsonObject,
) -> Result<bool> {
	if pdu
		.get("type")
		.and_then(CanonicalJsonValue::as_str)
		!= Some("m.room.member")
	{
		return Ok(false);
	}

	let Some(target) = pdu
		.get("state_key")
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|state_key| UserId::parse(state_key).ok())
	else {
		return Ok(false);
	};

	let Some(sender) = pdu
		.get("sender")
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|sender| UserId::parse(sender).ok())
	else {
		return Ok(false);
	};

	if sender == target || !self.services.globals.user_is_local(&target) {
		return Ok(false);
	}

	let Some(content) = pdu
		.get("content")
		.cloned()
		.map(Into::into)
		.and_then(|content| serde_json::from_value::<RoomMemberEventContent>(content).ok())
	else {
		return Ok(false);
	};

	if content.membership != MembershipState::Leave {
		return Ok(false);
	}

	if self
		.services
		.state_cache
		.user_membership(&target, room_id)
		.ne(&Some(MembershipState::Invite))
		.await
	{
		return Ok(false);
	}

	// Recover the inviter and the room version from the stored stripped state.
	let invite_state = self
		.services
		.state_cache
		.invite_state(&target, room_id)
		.await?;

	let inviter = invite_state
		.iter()
		.find_map(|event| match event.deserialize() {
			| Ok(AnyStrippedStateEvent::RoomMember(member)) if member.state_key == target =>
				Some(member.sender),
			| _ => None,
		});

	// Honor the rescission only from the original inviter.
	if inviter.as_ref() != Some(&sender) {
		return Ok(false);
	}

	let Some(room_version_id) = room_version_of(&invite_state) else {
		return Ok(false);
	};

	// Verify the kick is signed by the sender's server before acting on it.
	self.services
		.server_keys
		.verify_event(pdu, Some(&room_version_id))
		.await
		.map_err(|e| {
			err!(Request(InvalidParam("Invite rescission signature is invalid: {e}")))
		})?;

	let count = self.services.globals.next_count();
	self.services
		.state_cache
		.update_membership(MembershipUpdate {
			room_id,
			user_id: &target,
			membership_event: RoomMemberEventContent::new(MembershipState::Leave),
			sender: &sender,
			last_state: None,
			invite_via: None,
			update_joined_count: false,
			count: PduCount::Normal(*count),
		})
		.await?;

	debug!(%room_id, %target, %sender, "Applied a federated invite rescission.");

	Ok(true)
}

/// Upgrade an incoming PDU's previous events, walking interior events after
/// their parents so each derives state locally instead of refetching it.
///
/// Extremities upgrade concurrently up to `prev_events_concurrency`; interior
/// events upgrade one at a time. Returns how many previous events were upgraded.
#[implement(super::Service)]
async fn handle_prev_events(
	&self,
	upgrade: PrevUpgrade<'_>,
	sorted_prev_events: Vec<OwnedEventId>,
	mut pdus: Pdus, // HashMap::remove takes &mut self
) -> Result<usize> {
	trace!(
		events = sorted_prev_events.len(),
		event_ids = ?sorted_prev_events,
		"Handling previous events"
	);

	let (interior, extremities): (PrevSplit, PrevSplit) = sorted_prev_events
		.into_iter()
		.partition(|prev_id| {
			pdus.get(prev_id).is_some_and(|(pdu, _)| {
				pdu.prev_events()
					.any(|prev| pdus.contains_key(prev))
			})
		});

	let concurrency = usize::from(
		self.services
			.server
			.config
			.prev_events_concurrency,
	);

	let upgraded = extremities
		.into_iter()
		.try_stream()
		.map_ok(|prev_id| (pdus.remove(&prev_id), prev_id))
		.broadn_and_then(concurrency, async |(info, prev_id)| {
			self.upgrade_prev_event(upgrade, info, &prev_id)
				.await
		})
		.ready_try_fold(0, tally_upgraded)
		.boxed() // size firewall
		.await?;

	// Walk interior events forward so each parent commits before its children.
	interior
		.into_iter()
		.try_stream()
		.map_ok(|prev_id| (pdus.remove(&prev_id), prev_id))
		.and_then(async |(info, prev_id)| {
			self.upgrade_prev_event(upgrade, info, &prev_id)
				.await
		})
		.ready_try_fold(upgraded, tally_upgraded)
		.boxed() // size firewall
		.await
}

/// Upgrade one previous event, folding a failure into a non-fatal skip so a
/// single bad prev does not abort the batch.
///
/// A failure records a transient backoff for the prev. A shutdown propagates
/// through the leading running check; an interruption surfacing from inside
/// the upgrade is not a verdict on the prev and records nothing.
#[implement(super::Service)]
async fn upgrade_prev_event(
	&self,
	upgrade: PrevUpgrade<'_>,
	info: Option<(PduEvent, CanonicalJsonObject)>,
	prev_id: &EventId,
) -> Result<Handled> {
	self.services.server.check_running()?;

	let PrevUpgrade { room_id, event_id, .. } = upgrade;

	match self.handle_prev_pdu(upgrade, info, prev_id).await {
		| Err(error) if error.is_interrupted() || self.services.server.is_stopping() => {
			debug!(?prev_id, ?event_id, ?room_id, %error, "Prev event processing interrupted.");

			Ok(None)
		},
		| Err(error) => {
			self.record_outcome(Context::Upgrade, prev_id, Disposition::Transient);
			warn!(?prev_id, ?event_id, ?room_id, %error, "Prev event processing failed.");

			Ok(None)
		},
		| Ok(None) => {
			debug_warn!(?prev_id, "Prev event not processed.");

			Ok(None)
		},
		| Ok(handled) => {
			self.record_success(Context::Upgrade, prev_id)
				.await;

			debug!(?prev_id, ?handled, "Prev event processed.");

			Ok(handled)
		},
	}
}

fn tally_upgraded(upgraded: usize, handled: Handled) -> Result<usize> {
	Ok(upgraded.saturating_add(usize::from(handled.is_some())))
}
