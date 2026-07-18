use std::collections::BTreeMap;

use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};

use super::timer::TypingTimer;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TypingEntry {
	pub(super) expires_at_ms: u64,
	/// Distinguishes refreshes, including refreshes with the same deadline.
	pub(super) generation: u64,
}

#[derive(Default)]
pub(super) struct TypingState {
	pub(super) rooms: BTreeMap<OwnedRoomId, BTreeMap<OwnedUserId, TypingEntry>>,
	next_generation: u64,
}

#[must_use = "typing transitions include timer commands which must be queued"]
pub(super) struct StateTransition<T> {
	pub(super) output: T,
	pub(super) timers: Vec<TypingTimer>,
}

impl TypingState {
	pub(super) fn start(
		&mut self,
		room_id: &RoomId,
		user_id: &UserId,
		expires_at_ms: u64,
	) -> StateTransition<bool> {
		self.next_generation = self
			.next_generation
			.checked_add(1)
			.expect("typing generation overflow");
		let entry = TypingEntry {
			expires_at_ms,
			generation: self.next_generation,
		};
		let started = self
			.rooms
			.entry(room_id.to_owned())
			.or_default()
			.insert(user_id.to_owned(), entry)
			.is_none();

		StateTransition {
			output: started,
			timers: vec![TypingTimer::Add {
				room_id: room_id.to_owned(),
				user_id: user_id.to_owned(),
				entry,
			}],
		}
	}

	pub(super) fn stop(&mut self, room_id: &RoomId, user_id: &UserId) -> StateTransition<bool> {
		let Some(entry) = self
			.rooms
			.get_mut(room_id)
			.and_then(|room| room.remove(user_id))
		else {
			return StateTransition { output: false, timers: Vec::new() };
		};
		self.prune(room_id);

		StateTransition {
			output: true,
			timers: vec![TypingTimer::Remove {
				room_id: room_id.to_owned(),
				user_id: user_id.to_owned(),
				generation: entry.generation,
			}],
		}
	}

	pub(super) fn expire(
		&mut self,
		room_id: &RoomId,
		user_id: &UserId,
		expected_entry: TypingEntry,
		now_ms: u64,
	) -> StateTransition<bool> {
		let Some(entry) = self
			.rooms
			.get(room_id)
			.and_then(|room| room.get(user_id))
			.copied()
		else {
			return StateTransition { output: false, timers: Vec::new() };
		};
		if entry != expected_entry || entry.expires_at_ms > now_ms {
			return StateTransition { output: false, timers: Vec::new() };
		}

		self.stop(room_id, user_id)
	}

	pub(super) fn expire_room(
		&mut self,
		room_id: &RoomId,
		now_ms: u64,
	) -> StateTransition<Vec<OwnedUserId>> {
		let expired = self
			.rooms
			.get(room_id)
			.into_iter()
			.flat_map(BTreeMap::iter)
			.filter_map(|(user_id, entry)| {
				(entry.expires_at_ms <= now_ms).then(|| user_id.clone())
			})
			.collect::<Vec<_>>();
		let timers = expired
			.iter()
			.flat_map(|user_id| self.stop(room_id, user_id).timers)
			.collect();

		StateTransition { output: expired, timers }
	}

	pub(super) fn users(&self, room_id: &RoomId) -> Vec<OwnedUserId> {
		self.rooms
			.get(room_id)
			.into_iter()
			.flat_map(BTreeMap::keys)
			.cloned()
			.collect()
	}

	fn prune(&mut self, room_id: &RoomId) {
		if self
			.rooms
			.get(room_id)
			.is_some_and(BTreeMap::is_empty)
		{
			self.rooms.remove(room_id);
		}
	}
}
