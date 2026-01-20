//! Deferred push suppression queues.
//!
//! Stores suppressed push events in memory until they can be flushed. This is
//! intentionally in-memory only: suppressed events are discarded on restart.

use std::{
	collections::{HashMap, VecDeque},
	sync::Mutex,
};

use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use tuwunel_core::{debug, implement, trace, utils};

use crate::rooms::timeline::RawPduId;

const SUPPRESSED_MAX_EVENTS_PER_ROOM: usize = 512;
const SUPPRESSED_MAX_EVENTS_PER_PUSHKEY: usize = 4096;
const SUPPRESSED_MAX_ROOMS_PER_PUSHKEY: usize = 256;

type SuppressedRooms = Vec<(OwnedRoomId, Vec<RawPduId>)>;
type SuppressedPushes = Vec<(String, SuppressedRooms)>;

#[derive(Default)]
pub(super) struct SuppressedQueue {
	inner: Mutex<HashMap<OwnedUserId, HashMap<String, PushkeyQueue>>>,
}

#[derive(Default)]
struct PushkeyQueue {
	rooms: HashMap<OwnedRoomId, VecDeque<SuppressedEvent>>,
	total_events: usize,
}

#[derive(Clone, Debug)]
struct SuppressedEvent {
	pdu_id: RawPduId,
	_inserted_at_ms: u64,
}

impl SuppressedQueue {
	fn lock(
		&self,
	) -> std::sync::MutexGuard<'_, HashMap<OwnedUserId, HashMap<String, PushkeyQueue>>> {
		self.inner
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	fn drain_room(queue: VecDeque<SuppressedEvent>) -> Vec<RawPduId> {
		queue
			.into_iter()
			.map(|event| event.pdu_id)
			.collect()
	}

	fn drop_one_front(queue: &mut VecDeque<SuppressedEvent>, total_events: &mut usize) -> bool {
		if queue.pop_front().is_some() {
			*total_events = total_events.saturating_sub(1);
			return true;
		}

		false
	}
}

/// Enqueue a PDU for later push delivery when suppression is active.
#[implement(super::Service)]
pub fn queue_suppressed_push(
	&self,
	user_id: &UserId,
	pushkey: &str,
	room_id: &RoomId,
	pdu_id: RawPduId,
) -> bool {
	let mut inner = self.suppressed.lock();
	let user_entry = inner.entry(user_id.to_owned()).or_default();
	let push_entry = user_entry.entry(pushkey.to_owned()).or_default();

	if !push_entry.rooms.contains_key(room_id)
		&& push_entry.rooms.len() >= SUPPRESSED_MAX_ROOMS_PER_PUSHKEY
	{
		debug!(
			?user_id,
			?room_id,
			pushkey,
			max_rooms = SUPPRESSED_MAX_ROOMS_PER_PUSHKEY,
			"Suppressed push queue full (rooms); dropping event"
		);
		return false;
	}

	let queue = push_entry
		.rooms
		.entry(room_id.to_owned())
		.or_default();

	if queue
		.back()
		.is_some_and(|event| event.pdu_id == pdu_id)
	{
		trace!(?user_id, ?room_id, pushkey, "Suppressed push event is duplicate; skipping");
		return false;
	}

	if push_entry.total_events >= SUPPRESSED_MAX_EVENTS_PER_PUSHKEY && queue.is_empty() {
		debug!(
			?user_id,
			?room_id,
			pushkey,
			max_events = SUPPRESSED_MAX_EVENTS_PER_PUSHKEY,
			"Suppressed push queue full (total); dropping event"
		);
		return false;
	}

	while queue.len() >= SUPPRESSED_MAX_EVENTS_PER_ROOM
		|| push_entry.total_events >= SUPPRESSED_MAX_EVENTS_PER_PUSHKEY
	{
		if !SuppressedQueue::drop_one_front(queue, &mut push_entry.total_events) {
			break;
		}
	}

	queue.push_back(SuppressedEvent {
		pdu_id,
		_inserted_at_ms: utils::millis_since_unix_epoch(),
	});
	push_entry.total_events = push_entry.total_events.saturating_add(1);

	true
}

/// Take and remove all suppressed PDUs for a given user + pushkey.
#[implement(super::Service)]
pub fn take_suppressed_for_pushkey(
	&self,
	user_id: &UserId,
	pushkey: &str,
) -> Vec<(OwnedRoomId, Vec<RawPduId>)> {
	let mut inner = self.suppressed.lock();
	let Some(user_entry) = inner.get_mut(user_id) else {
		return Vec::new();
	};

	let Some(push_entry) = user_entry.remove(pushkey) else {
		return Vec::new();
	};

	if user_entry.is_empty() {
		inner.remove(user_id);
	}

	push_entry
		.rooms
		.into_iter()
		.map(|(room_id, queue)| (room_id, SuppressedQueue::drain_room(queue)))
		.collect()
}

/// Take and remove all suppressed PDUs for a given user across all pushkeys.
#[implement(super::Service)]
pub fn take_suppressed_for_user(&self, user_id: &UserId) -> SuppressedPushes {
	let mut inner = self.suppressed.lock();
	let Some(user_entry) = inner.remove(user_id) else {
		return Vec::new();
	};

	user_entry
		.into_iter()
		.map(|(pushkey, queue)| {
			let rooms = queue
				.rooms
				.into_iter()
				.map(|(room_id, q)| (room_id, SuppressedQueue::drain_room(q)))
				.collect();
			(pushkey, rooms)
		})
		.collect()
}

/// Clear suppressed PDUs for a specific room (across all pushkeys).
#[implement(super::Service)]
pub fn clear_suppressed_room(&self, user_id: &UserId, room_id: &RoomId) -> usize {
	let mut inner = self.suppressed.lock();
	let Some(user_entry) = inner.get_mut(user_id) else {
		return 0;
	};

	let mut removed: usize = 0;
	user_entry.retain(|_, push_entry| {
		if let Some(queue) = push_entry.rooms.remove(room_id) {
			removed = removed.saturating_add(queue.len());
			push_entry.total_events = push_entry
				.total_events
				.saturating_sub(queue.len());
		}

		!push_entry.rooms.is_empty()
	});

	if user_entry.is_empty() {
		inner.remove(user_id);
	}

	removed
}

/// Clear suppressed PDUs for a specific pushkey.
#[implement(super::Service)]
pub fn clear_suppressed_pushkey(&self, user_id: &UserId, pushkey: &str) -> usize {
	let mut inner = self.suppressed.lock();
	let Some(user_entry) = inner.get_mut(user_id) else {
		return 0;
	};

	let removed = user_entry
		.remove(pushkey)
		.map(|queue| queue.total_events)
		.unwrap_or(0);

	if user_entry.is_empty() {
		inner.remove(user_id);
	}

	removed
}
