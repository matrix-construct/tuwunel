use std::time::Duration;

use ruma::{room_id, user_id};

use super::{
	state::{TypingEntry, TypingState},
	timer::{FiredTimer, TimerKey, TimerQueue, TypingTimer, typing_timer},
};

fn entry(expires_at_ms: u64, generation: u64) -> TypingEntry {
	TypingEntry { expires_at_ms, generation }
}

fn add(key: &TimerKey, entry: TypingEntry) -> TypingTimer {
	TypingTimer::Add {
		room_id: key.0.clone(),
		user_id: key.1.clone(),
		entry,
	}
}

fn remove(key: &TimerKey, generation: u64) -> TypingTimer {
	TypingTimer::Remove {
		room_id: key.0.clone(),
		user_id: key.1.clone(),
		generation,
	}
}

fn timer_key() -> TimerKey {
	(
		room_id!("!room:example.com").to_owned(),
		user_id!("@alice:example.com").to_owned(),
	)
}

#[test]
fn timer_queue_stop_then_restart_keeps_restart() {
	let key = timer_key();
	let first = entry(1000, 1);
	let restarted = entry(2000, 2);
	let mut queue = TimerQueue::default();

	let _ = queue.update(add(&key, first));
	let _ = queue.update(remove(&key, first.generation));
	let _ = queue.update(add(&key, restarted));

	assert_eq!(queue.current(&key), Some(restarted));
}

#[test]
fn timer_queue_refresh_replaces_previous_generation() {
	let key = timer_key();
	let first = entry(1000, 1);
	let refreshed = entry(2000, 2);
	let mut queue = TimerQueue::default();

	let _ = queue.update(add(&key, first));
	let _ = queue.update(add(&key, refreshed));
	// A late cancellation for the old generation must not cancel the refresh.
	let _ = queue.update(remove(&key, first.generation));

	assert_eq!(queue.current(&key), Some(refreshed));
}

#[test]
fn stale_fired_timer_does_not_consume_current_timer() {
	let key = timer_key();
	let stale = entry(1000, 1);
	let current = entry(2000, 2);
	let mut queue = TimerQueue::default();

	let _ = queue.update(add(&key, stale));
	let _ = queue.update(add(&key, current));

	assert!(matches!(queue.fired(&key, stale, 1000), FiredTimer::Stale));
	assert_eq!(queue.current(&key), Some(current));
}

#[test]
fn matching_timer_is_rescheduled_until_wall_deadline() {
	let key = timer_key();
	let current = entry(1000, 1);
	let mut queue = TimerQueue::default();

	let _ = queue.update(add(&key, current));
	assert!(matches!(queue.fired(&key, current, 999), FiredTimer::Reschedule(_)));
	assert_eq!(queue.current(&key), Some(current));

	assert!(matches!(queue.fired(&key, current, 1000), FiredTimer::Expire));
	assert_eq!(queue.current(&key), None);
}

#[test]
fn expired_typing_users_include_deadline_boundary_and_prune_room() {
	let room_id = room_id!("!room:example.com");
	let expired = user_id!("@expired:example.com");
	let boundary = user_id!("@boundary:example.com");
	let active = user_id!("@active:example.com");
	let mut state = TypingState::default();

	let _ = state.start(room_id, expired, 999);
	let _ = state.start(room_id, boundary, 1000);
	let _ = state.start(room_id, active, 1001);

	let transition = state.expire_room(room_id, 1000);
	let expired_users = transition.output;
	assert_eq!(expired_users, vec![boundary.to_owned(), expired.to_owned()]);
	assert_eq!(transition.timers.len(), 2);
	assert_eq!(state.users(room_id), vec![active.to_owned()]);

	let transition = state.expire_room(room_id, 1001);
	assert_eq!(transition.output, vec![active.to_owned()]);
	assert!(state.users(room_id).is_empty());
	assert!(!state.rooms.contains_key(room_id));
}

#[test]
fn stale_timer_does_not_remove_refreshed_typing_even_with_same_deadline() {
	let room_id = room_id!("!room:example.com");
	let user_id = user_id!("@alice:example.com");
	let mut state = TypingState::default();

	let first_timer = state
		.start(room_id, user_id, 1000)
		.timers
		.remove(0);
	let second_timer = state
		.start(room_id, user_id, 1000)
		.timers
		.remove(0);
	let TypingTimer::Add { entry: first_entry, .. } = first_timer else {
		panic!("start should schedule a timer");
	};
	let TypingTimer::Add { entry: second_entry, .. } = second_timer else {
		panic!("refresh should schedule a timer");
	};

	assert_ne!(first_entry.generation, second_entry.generation);
	assert!(
		!state
			.expire(room_id, user_id, first_entry, 1000)
			.output
	);
	assert_eq!(state.rooms[room_id][user_id], second_entry);
	assert!(
		state
			.expire(room_id, user_id, second_entry, 1000)
			.output
	);
	assert!(state.users(room_id).is_empty());
}

#[test]
fn stop_then_restart_uses_generation_qualified_timer_commands() {
	let room_id = room_id!("!room:example.com");
	let user_id = user_id!("@alice:example.com");
	let mut state = TypingState::default();

	let first_timer = state
		.start(room_id, user_id, 1000)
		.timers
		.remove(0);
	let remove_timer = state
		.stop(room_id, user_id)
		.timers
		.pop()
		.expect("active typing should stop");
	let restarted_timer = state
		.start(room_id, user_id, 2000)
		.timers
		.remove(0);

	let TypingTimer::Add { entry: first_entry, .. } = first_timer else {
		panic!("start should schedule a timer");
	};
	let TypingTimer::Remove { generation: removed_generation, .. } = remove_timer else {
		panic!("stop should cancel a timer");
	};
	let TypingTimer::Add { entry: restarted_entry, .. } = restarted_timer else {
		panic!("restart should schedule a timer");
	};

	assert_eq!(removed_generation, first_entry.generation);
	assert_ne!(removed_generation, restarted_entry.generation);
	assert_eq!(state.rooms[room_id][user_id], restarted_entry);
}

#[tokio::test]
async fn elapsed_timer_can_expire_state_without_a_sync_read() {
	let room_id = room_id!("!room:example.com").to_owned();
	let user_id = user_id!("@alice:example.com").to_owned();
	let expires_at_ms = tuwunel_core::utils::millis_since_unix_epoch().saturating_sub(1);
	let mut state = TypingState::default();
	let timer = state
		.start(&room_id, &user_id, expires_at_ms)
		.timers
		.remove(0);
	let TypingTimer::Add { entry, .. } = timer else {
		panic!("start should schedule a timer");
	};

	let (_, _, fired_entry) = tokio::time::timeout(
		Duration::from_millis(50),
		typing_timer(room_id.clone(), user_id.clone(), entry),
	)
	.await
	.expect("elapsed typing timer should fire immediately");

	assert_eq!(fired_entry, entry);
	assert!(
		state
			.expire(
				&room_id,
				&user_id,
				fired_entry,
				tuwunel_core::utils::millis_since_unix_epoch(),
			)
			.output
	);
	assert!(state.users(&room_id).is_empty());
}

#[test]
fn matching_timer_only_expires_at_or_after_deadline() {
	let room_id = room_id!("!room:example.com");
	let user_id = user_id!("@alice:example.com");
	let mut state = TypingState::default();
	let timer = state
		.start(room_id, user_id, 1000)
		.timers
		.remove(0);
	let TypingTimer::Add { entry, .. } = timer else {
		panic!("start should schedule a timer");
	};

	assert!(!state.expire(room_id, user_id, entry, 999).output);
	assert_eq!(state.rooms[room_id][user_id], TypingEntry {
		expires_at_ms: 1000,
		generation: entry.generation,
	});
	assert!(state.expire(room_id, user_id, entry, 1000).output);
}
