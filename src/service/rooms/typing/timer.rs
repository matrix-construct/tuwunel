use std::{collections::BTreeMap, time::Duration};

use futures::future::{AbortHandle, AbortRegistration};
use ruma::{OwnedRoomId, OwnedUserId};
use tuwunel_core::utils;

use super::state::TypingEntry;

pub(super) type TimerKey = (OwnedRoomId, OwnedUserId);
pub(super) type TimerFired = (OwnedRoomId, OwnedUserId, TypingEntry);

#[derive(Default)]
pub(super) struct TimerQueue {
	handles: BTreeMap<TimerKey, (TypingEntry, AbortHandle)>,
}

pub(super) struct ScheduledTimer {
	pub(super) room_id: OwnedRoomId,
	pub(super) user_id: OwnedUserId,
	pub(super) entry: TypingEntry,
	pub(super) registration: AbortRegistration,
}

pub(super) enum FiredTimer {
	Stale,
	Reschedule(AbortRegistration),
	Expire,
}

#[derive(Debug)]
pub(super) enum TypingTimer {
	Add {
		room_id: OwnedRoomId,
		user_id: OwnedUserId,
		entry: TypingEntry,
	},
	Remove {
		room_id: OwnedRoomId,
		user_id: OwnedUserId,
		generation: u64,
	},
}

impl TimerQueue {
	pub(super) fn update(&mut self, timer: TypingTimer) -> Option<ScheduledTimer> {
		match timer {
			| TypingTimer::Add { room_id, user_id, entry } => {
				let key = (room_id.clone(), user_id.clone());
				let registration = self.schedule(key, entry);

				Some(ScheduledTimer { room_id, user_id, entry, registration })
			},
			| TypingTimer::Remove { room_id, user_id, generation } => {
				let key = (room_id, user_id);
				if self
					.handles
					.get(&key)
					.is_some_and(|(entry, _)| entry.generation == generation)
					&& let Some((_, handle)) = self.handles.remove(&key)
				{
					handle.abort();
				}

				None
			},
		}
	}

	pub(super) fn fired(
		&mut self,
		key: &TimerKey,
		entry: TypingEntry,
		now_ms: u64,
	) -> FiredTimer {
		if !self
			.handles
			.get(key)
			.is_some_and(|(current_entry, _)| *current_entry == entry)
		{
			return FiredTimer::Stale;
		}

		self.handles.remove(key);
		if now_ms < entry.expires_at_ms {
			FiredTimer::Reschedule(self.schedule(key.clone(), entry))
		} else {
			FiredTimer::Expire
		}
	}

	fn schedule(&mut self, key: TimerKey, entry: TypingEntry) -> AbortRegistration {
		if let Some((_, handle)) = self.handles.remove(&key) {
			handle.abort();
		}

		let (handle, registration) = AbortHandle::new_pair();
		self.handles.insert(key, (entry, handle));

		registration
	}

	#[cfg(test)]
	pub(super) fn current(&self, key: &TimerKey) -> Option<TypingEntry> {
		self.handles.get(key).map(|(entry, _)| *entry)
	}
}

pub(super) async fn typing_timer(
	room_id: OwnedRoomId,
	user_id: OwnedUserId,
	entry: TypingEntry,
) -> TimerFired {
	let delay = entry
		.expires_at_ms
		.saturating_sub(utils::millis_since_unix_epoch());
	tokio::time::sleep(Duration::from_millis(delay)).await;

	(room_id, user_id, entry)
}
