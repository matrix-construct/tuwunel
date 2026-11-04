use std::{iter::once, pin::pin, task::Waker, time::Duration};

use ruma::{RoomId, room_id, user_id};
use tokio::time::timeout;
use tuwunel_core::{
	Result,
	config::Figment,
	utils::{IterStream, time::now_millis},
};

use crate::{
	Services,
	test_utils::{fixture, poll_once},
};

/// Typing updates that overflow the channel.
///
/// Its capacity of 100 rounds up to 128, so a flood of 100 would not lag.
const LAGGING_UPDATES: usize = 129;

/// How long a typist in these tests stays typing, in milliseconds.
const TYPING_FOR: u64 = 60_000;

/// How long a watch has to wake.
///
/// Only a failing test waits it out.
const WAKE_WINDOW: Duration = Duration::from_secs(1);

/// A watch outlives lag over other rooms, and its receiver still hears this one.
///
/// Every typing update on the server reaches every receiver, so this lag says
/// nothing about the room, and a long poll must not end on it.
#[tokio::test]
async fn lag_over_other_rooms_keeps_the_watch_waiting() -> Result {
	with_watched_room(async |services, room| {
		let watcher = watch_room(services, room).await;
		let mut watcher = pin!(watcher); // Pin::as_mut takes &mut self.

		lag_receivers(services);

		assert!(
			poll_once(watcher.as_mut(), Waker::noop()).is_pending(),
			"lag over other rooms ended the watch"
		);

		send_typing(services, room);

		assert!(
			timeout(WAKE_WINDOW, watcher).await.is_ok(),
			"the room's update went unheard after the lag"
		);

		Ok(())
	})
	.await
}

/// A typing update lost to lag still wakes the watch.
///
/// The message naming the room is gone, so only the room's stored update can
/// show that it moved.
#[tokio::test]
async fn lag_over_this_room_wakes_the_watch() -> Result {
	with_watched_room(async |services, room| {
		let watcher = watch_room(services, room).await;

		start_typing(services, room, now_millis().saturating_add(TYPING_FOR)).await?;
		lag_receivers(services);

		assert!(
			timeout(WAKE_WINDOW, watcher).await.is_ok(),
			"the watch slept through an update it lost to lag"
		);

		Ok(())
	})
	.await
}

/// Lag does not wake a watch for an update from before it registered.
///
/// The pass after `watch` reads past that update, so a wake would answer the
/// long poll with nothing new.
#[tokio::test]
async fn typing_before_the_watch_does_not_wake_it_on_lag() -> Result {
	with_watched_room(async |services, room| {
		start_typing(services, room, now_millis().saturating_add(TYPING_FOR)).await?;

		let watcher = watch_room(services, room).await;
		let watcher = pin!(watcher);

		lag_receivers(services);

		assert!(
			poll_once(watcher, Waker::noop()).is_pending(),
			"lag woke the watch for an update it predates"
		);

		Ok(())
	})
	.await
}

/// Lag wakes a watch for a typist past their timeout, whatever `since` says.
///
/// Only the pass after a wake removes an expired typist, and it is what tells
/// the room and federation that they stopped.
#[tokio::test]
async fn expired_typing_wakes_the_watch_on_lag() -> Result {
	with_watched_room(async |services, room| {
		start_typing(services, room, now_millis().saturating_sub(1)).await?;

		let watcher = watch_room(services, room).await;

		lag_receivers(services);

		assert!(
			timeout(WAKE_WINDOW, watcher).await.is_ok(),
			"the watch slept through a lag with an expired typist"
		);

		Ok(())
	})
	.await
}

/// Runs a test against a room a watch can register on.
///
/// `watch` skips a room with no short ID, and its typing receiver with it.
async fn with_watched_room<Test>(test: Test) -> Result
where
	Test: AsyncFnOnce(&Services, &RoomId) -> Result,
{
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!watched:localhost");

	services
		.short
		.get_or_create_shortroomid(room)
		.await;

	test(services, room).await
}

async fn watch_room<'a>(
	services: &'a Services,
	room: &'a RoomId,
) -> impl Future<Output = ()> + Send + 'a {
	services
		.sync
		.watch(user_id!("@alice:localhost"), None, once(room).stream())
		.await
}

fn lag_receivers(services: &Services) {
	for _ in 0..LAGGING_UPDATES {
		send_typing(services, room_id!("!other:localhost"));
	}
}

fn send_typing(services: &Services, room: &RoomId) {
	services
		.typing
		.typing_update_sender
		.send(room.to_owned())
		.expect("the watch holds a receiver");
}

/// Starts a remote user typing.
///
/// A remote typist reaches no federation send.
async fn start_typing(services: &Services, room: &RoomId, deadline: u64) -> Result {
	let typist = user_id!("@typist:remote.example");

	services
		.typing
		.typing_add(typist, room, deadline)
		.await
}
