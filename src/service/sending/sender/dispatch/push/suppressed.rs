use futures::StreamExt;
use ruma::{
	OwnedRoomId, OwnedUserId, RoomId, UserId, api::client::push::Pusher, presence::PresenceState,
	push::Ruleset,
};
use tuwunel_core::{
	Event, debug, extract_variant, implement,
	matrix::Pdu,
	trace,
	utils::{
		IterStream, ReadyExt,
		stream::{BroadbandExt, WidebandExt},
	},
	warn,
};

use super::PUSH_WIDTH;
use crate::{
	pusher::SuppressedRooms,
	rooms::timeline::RawPduId,
	sending::{SendingEvent, Service},
};

/// The pusher and ruleset one suppressed-push flush delivers with.
///
/// Every room and PDU of the flush is sent under the same values.
struct Flush<'a> {
	user_id: &'a UserId,
	pushkey: &'a str,
	pusher: &'a Pusher,
	ruleset: &'a Ruleset,
	reason: &'static str,
}

// One presence heartbeat and one sync long-poll respectively, plus margin.
const ACTIVE_PRESENCE_AGE_MS: u64 = 65_000;
const ACTIVE_SYNC_GAP_MS: u64 = 32_000;

/// Schedule a flush of the pushes suppressed for one pushkey.
///
/// The flush runs as a task this service owns, so the caller never waits on
/// the push gateway.
#[implement(Service)]
pub fn schedule_flush_suppressed_for_pushkey(
	&self,
	user_id: OwnedUserId,
	pushkey: String,
	reason: &'static str,
) {
	let sending = self.services.sending.clone();

	self.spawn_flush(async move {
		sending
			.flush_suppressed_for_pushkey(&user_id, &pushkey, reason)
			.await;
	});
}

/// Schedule a flush of the pushes suppressed for every pushkey a user owns.
///
/// The flush runs as a task this service owns, so the caller never waits on
/// the push gateway.
#[implement(Service)]
pub fn schedule_flush_suppressed_for_user(&self, user_id: OwnedUserId, reason: &'static str) {
	let sending = self.services.sending.clone();

	self.spawn_flush(async move {
		sending
			.flush_suppressed_for_user(&user_id, reason)
			.await;
	});
}

#[implement(Service)]
async fn flush_suppressed_for_pushkey(
	&self,
	user_id: &UserId,
	pushkey: &str,
	reason: &'static str,
) {
	let suppressed = self
		.services
		.pusher
		.take_suppressed_for_pushkey(user_id, pushkey);

	if suppressed.is_empty() {
		return;
	}

	let Ok(pusher) = self
		.services
		.pusher
		.get_pusher(user_id, pushkey)
		.await
		.inspect_err(|error| {
			warn!(?user_id, pushkey, ?error, "Missing pusher for suppressed flush");
		})
	else {
		return;
	};

	let ruleset = self.services.pusher.ruleset(user_id).await;
	let flush = Flush {
		user_id,
		pushkey,
		pusher: &pusher,
		ruleset: &ruleset,
		reason,
	};

	self.flush_suppressed_rooms(&flush, suppressed)
		.await;
}

#[implement(Service)]
async fn flush_suppressed_for_user(&self, user_id: &UserId, reason: &'static str) {
	let suppressed = self
		.services
		.pusher
		.take_suppressed_for_user(user_id);

	if suppressed.is_empty() {
		return;
	}

	let ruleset = self.services.pusher.ruleset(user_id).await;

	for (pushkey, rooms) in suppressed {
		let Ok(pusher) = self
			.services
			.pusher
			.get_pusher(user_id, &pushkey)
			.await
			.inspect_err(|error| {
				warn!(?user_id, pushkey, ?error, "Missing pusher for suppressed flush");
			})
		else {
			continue;
		};

		let flush = Flush {
			user_id,
			pushkey: &pushkey,
			pusher: &pusher,
			ruleset: &ruleset,
			reason,
		};

		self.flush_suppressed_rooms(&flush, rooms).await;
	}
}

#[implement(Service)]
async fn flush_suppressed_rooms(&self, flush: &Flush<'_>, rooms: SuppressedRooms) {
	if rooms.is_empty() {
		return;
	}

	let Flush { user_id, pushkey, reason, .. } = *flush;

	debug!(?user_id, pushkey, rooms = rooms.len(), reason, "Flushing suppressed pushes");
	let sent = rooms
		.into_iter()
		.stream()
		.then(|(room_id, pdu_ids)| self.flush_suppressed_room(flush, room_id, pdu_ids))
		.ready_fold(0, usize::saturating_add)
		.await;

	debug!(?user_id, pushkey, sent, "Flushed suppressed push notifications");
}

#[implement(Service)]
async fn flush_suppressed_room(
	&self,
	flush: &Flush<'_>,
	room_id: OwnedRoomId,
	pdu_ids: Vec<RawPduId>,
) -> usize {
	let unread = self
		.services
		.pusher
		.notification_count(flush.user_id, &room_id)
		.await;

	if unread == 0 {
		trace!(user_id = ?flush.user_id, ?room_id, "Skipping suppressed push flush: no unread");
		return 0;
	}

	pdu_ids
		.into_iter()
		.stream()
		.wide_filter_map(async |pdu_id| {
			self.suppressable_pdu(flush.user_id, &pdu_id)
				.await
				.map(|pdu| (pdu_id, pdu))
		})
		.broadn_then(Some(PUSH_WIDTH), async |(pdu_id, pdu)| {
			self.flush_suppressed_pdu(flush, &room_id, pdu_id, &pdu)
				.await
		})
		.ready_filter(|&sent| sent)
		.count()
		.await
}

#[implement(Service)]
async fn flush_suppressed_pdu(
	&self,
	flush: &Flush<'_>,
	room_id: &RoomId,
	pdu_id: RawPduId,
	pdu: &Pdu,
) -> bool {
	let Flush { user_id, pushkey, pusher, ruleset, .. } = *flush;

	let Err(error) = self
		.services
		.pusher
		.send_push_notice(user_id, pusher, ruleset, pdu)
		.await
	else {
		return true;
	};

	let requeued = self
		.services
		.pusher
		.queue_suppressed_push(user_id, pushkey, room_id, pdu_id);

	warn!(
		?user_id,
		?room_id,
		?error,
		requeued,
		"Failed to send suppressed push notification"
	);

	false
}

#[implement(Service)]
pub(super) async fn enqueue_suppressed_push_events(
	&self,
	user_id: &UserId,
	pushkey: &str,
	events: &[SendingEvent],
) -> usize {
	events
		.iter()
		.stream()
		.ready_filter_map(|event| extract_variant!(event, SendingEvent::Pdu))
		.wide_filter_map(async |pdu_id| {
			self.suppressable_pdu(user_id, pdu_id)
				.await
				.map(|pdu| (pdu_id, pdu))
		})
		.ready_fold(0, |queued: usize, (pdu_id, pdu)| {
			let accepted = self.services.pusher.queue_suppressed_push(
				user_id,
				pushkey,
				pdu.room_id(),
				*pdu_id,
			);

			queued.saturating_add(usize::from(accepted))
		})
		.await
}

/// Load a suppressed PDU if a push for it is still worth sending.
///
/// A missing or redacted PDU is dropped with a log line and never notified.
#[implement(Service)]
async fn suppressable_pdu(&self, user_id: &UserId, pdu_id: &RawPduId) -> Option<Pdu> {
	let Ok(pdu) = self
		.services
		.timeline
		.get_pdu_from_id(pdu_id)
		.await
	else {
		debug!(?user_id, ?pdu_id, "Suppressed PDU is missing");
		return None;
	};

	if pdu.is_redacted() {
		trace!(?user_id, ?pdu_id, "Suppressed PDU is redacted");
		return None;
	}

	Some(pdu)
}

/// Decide whether pushes for a user are suppressed as active.
///
/// The heuristic combines the presence age and the most recent sync gap, and
/// only applies when `suppress_push_when_active` is enabled. An offline user
/// is never suppressed; the two `ACTIVE_*` constants are the thresholds.
#[implement(Service)]
pub(super) async fn pushing_suppressed(&self, user_id: &UserId) -> bool {
	if !self.services.config.suppress_push_when_active {
		debug!(?user_id, "push not suppressed: suppress_push_when_active disabled");
		return false;
	}

	let Ok(presence) = self.services.presence.get_presence(user_id).await else {
		debug!(?user_id, "push not suppressed: presence unavailable");
		return false;
	};

	if presence.content.presence != PresenceState::Online {
		debug!(
			?user_id,
			presence = ?presence.content.presence,
			"push not suppressed: presence not online"
		);

		return false;
	}

	let presence_age_ms = presence
		.content
		.last_active_ago
		.map(u64::from)
		.unwrap_or(u64::MAX);

	if presence_age_ms >= ACTIVE_PRESENCE_AGE_MS {
		debug!(?user_id, presence_age_ms, "push not suppressed: presence too old");
		return false;
	}

	let sync_gap_ms = self
		.services
		.presence
		.last_sync_gap_ms(user_id)
		.await;

	match sync_gap_ms {
		| Some(gap) if gap < ACTIVE_SYNC_GAP_MS => {
			debug!(
				?user_id,
				presence_age_ms,
				sync_gap_ms = gap,
				"suppressing push: active heuristic"
			);

			true
		},
		| Some(gap) => {
			debug!(
				?user_id,
				presence_age_ms,
				sync_gap_ms = gap,
				"push not suppressed: sync gap too large"
			);

			false
		},
		| None => {
			debug!(?user_id, presence_age_ms, "push not suppressed: no recent sync");

			false
		},
	}
}
