use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use futures::{
	FutureExt, StreamExt, TryStreamExt,
	future::{Abortable, Aborted, try_join},
	stream::FuturesUnordered,
};
use loole::{Receiver, Sender};
use ruma::{
	OwnedRoomId, OwnedUserId, RoomId, UserId,
	api::{
		appservice::event::push_events::v1::EphemeralData,
		federation::transactions::edu::{Edu, TypingContent},
	},
	events::{EphemeralRoomEvent, typing::TypingEventContent},
};
use tokio::sync::{RwLock, broadcast};
use tuwunel_core::{
	Result, Server, debug_error, debug_info,
	result::LogErr,
	trace,
	utils::{self, BoolExt, IterStream},
};

use crate::sending::EduBuf;

mod state;
mod timer;

#[cfg(test)]
mod tests;

use self::{
	state::{StateTransition, TypingEntry, TypingState},
	timer::{FiredTimer, ScheduledTimer, TimerFired, TimerQueue, TypingTimer, typing_timer},
};

pub struct Service {
	server: Arc<Server>,
	services: Arc<crate::services::OnceServices>,
	state: RwLock<TypingState>,
	/// timestamp of the last change to typing users
	last_typing_update: RwLock<BTreeMap<OwnedRoomId, u64>>,
	typing_update_sender: broadcast::Sender<OwnedRoomId>,
	timer_channel: (Sender<TypingTimer>, Receiver<TypingTimer>),
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			server: args.server.clone(),
			services: args.services.clone(),
			state: RwLock::new(TypingState::default()),
			last_typing_update: RwLock::new(BTreeMap::new()),
			typing_update_sender: broadcast::channel(100).0,
			timer_channel: loole::unbounded(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		let receiver = self.timer_channel.1.clone();
		let mut timers = FuturesUnordered::new();
		let mut timer_queue = TimerQueue::default();

		while !receiver.is_closed() && self.server.is_running() {
			tokio::select! {
				Some(result) = timers.next() => {
					let result: std::result::Result<TimerFired, Aborted> = result;
					let Ok((room_id, user_id, entry)) = result else {
						continue;
					};

					let key = (room_id.clone(), user_id);
					let now_ms = utils::millis_since_unix_epoch();
					match timer_queue.fired(&key, entry, now_ms) {
						| FiredTimer::Stale => {
							trace!(?room_id, ?entry, "Skipping stale typing timer");
						},
						| FiredTimer::Reschedule(registration) => {
							trace!(?room_id, ?entry, "Rescheduling typing timer after clock change");
							timers.push(Abortable::new(
								typing_timer(room_id, key.1.clone(), entry),
								registration,
							));
						},
						| FiredTimer::Expire => {
							self.expire_typing(&room_id, &key.1, entry, now_ms)
								.await
								.log_err()
								.ok();
						},
					}
				},
				event = receiver.recv_async() => match event {
					Ok(event) => {
						if let Some(ScheduledTimer {
							room_id,
							user_id,
							entry,
							registration,
						}) = timer_queue.update(event)
						{
							timers.push(Abortable::new(
								typing_timer(room_id, user_id, entry),
								registration,
							));
						}
					},
					Err(_) => break,
				},
			}
		}

		// Make future state transitions observe that active expiry is unavailable
		// instead of silently accumulating commands after the worker stops.
		receiver.close();
		Ok(())
	}

	async fn interrupt(&self) {
		if !self.timer_channel.0.is_closed() {
			self.timer_channel.0.close();
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Sets a user as typing until the timeout timestamp is reached or
	/// roomtyping_remove is called.
	pub async fn typing_add(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		expires_at_ms: u64,
	) -> Result {
		debug_info!("typing started {user_id:?} in {room_id:?} expires_at_ms:{expires_at_ms:?}");

		let started = self
			.transition_state(|state| state.start(room_id, user_id, expires_at_ms))
			.await;

		if started {
			self.announce_update(room_id).await;
			self.appservice_send(room_id).await?;
		}

		// Federation typing has no deadline, so a refresh must still be relayed
		// even though it is not an observer-visible local state transition.
		if self.services.globals.user_is_local(user_id) {
			self.federation_send(room_id, user_id, true)
				.await?;
		}

		Ok(())
	}

	/// Removes a user from typing before the timeout is reached.
	pub async fn typing_remove(&self, user_id: &UserId, room_id: &RoomId) -> Result {
		debug_info!("typing stopped {user_id:?} in {room_id:?}");

		if !self
			.transition_state(|state| state.stop(room_id, user_id))
			.await
		{
			return Ok(());
		}

		self.announce_update(room_id).await;
		let appservice_send = self.appservice_send(room_id);
		let federation_send = self
			.services
			.globals
			.user_is_local(user_id)
			.then_async(|| self.federation_send(room_id, user_id, false))
			.map(Option::transpose);

		try_join(appservice_send, federation_send)
			.await
			.map(|_| ())
	}

	pub async fn wait_for_update(&self, room_id: &RoomId) {
		let mut receiver = self.subscribe();
		while let Ok(next) = receiver.recv().await {
			if next == room_id {
				break;
			}
		}
	}

	/// Makes sure that typing events with old timestamps get removed.
	async fn typings_maintain(&self, room_id: &RoomId) -> Result {
		let current_timestamp = utils::millis_since_unix_epoch();
		let removable = self
			.transition_state(|state| state.expire_room(room_id, current_timestamp))
			.await;

		self.announce_expired(room_id, &removable).await
	}

	async fn expire_typing(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		expected_entry: TypingEntry,
		now_ms: u64,
	) -> Result {
		let removed = self
			.transition_state(|state| state.expire(room_id, user_id, expected_entry, now_ms))
			.await;

		if !removed {
			return Ok(());
		}

		let user_id = user_id.to_owned();
		self.announce_expired(room_id, std::slice::from_ref(&user_id))
			.await
	}

	async fn announce_expired(&self, room_id: &RoomId, users: &[OwnedUserId]) -> Result {
		if users.is_empty() {
			return Ok(());
		}

		for user_id in users {
			debug_info!("typing timeout {user_id:?} in {room_id:?}");
		}

		self.announce_update(room_id).await;

		let appservice_send = self.appservice_send(room_id);
		let federation_sends = users
			.iter()
			.filter(|user_id| self.services.globals.user_is_local(user_id))
			.try_stream()
			.try_for_each(|user_id| self.federation_send(room_id, user_id, false));

		try_join(appservice_send, federation_sends)
			.boxed()
			.await
			.map(|_| ())
	}

	async fn announce_update(&self, room_id: &RoomId) {
		let count = self.services.globals.next_count();
		self.last_typing_update
			.write()
			.await
			.insert(room_id.to_owned(), *count);
		drop(count);

		if self
			.typing_update_sender
			.send(room_id.to_owned())
			.is_err()
		{
			trace!("receiver found what it was looking for and is no longer interested");
		}
	}

	/// Returns the count of the last typing update in this room.
	pub async fn last_typing_update(&self, room_id: &RoomId) -> Result<u64> {
		self.typings_maintain(room_id).await?;

		self.last_typing_update
			.read()
			.await
			.get(room_id)
			.copied()
			.map(Ok)
			.unwrap_or(Ok(0))
	}

	/// Returns the typing content with all typing users in the room.
	async fn typings_content(&self, room_id: &RoomId) -> TypingEventContent {
		TypingEventContent {
			user_ids: self.state.read().await.users(room_id),
		}
	}

	/// Sends a typing EDU to all appservices interested in the room.
	async fn appservice_send(&self, room_id: &RoomId) -> Result {
		let content = self.typings_content(room_id).await;

		self.services
			.sending
			.send_edu_room_appservices(room_id, |buf| {
				let edu = EphemeralData::Typing(EphemeralRoomEvent {
					room_id: room_id.to_owned(),
					content: content.clone(),
				});

				Ok(serde_json::to_writer(buf, &edu)?)
			})
			.await
	}

	/// Returns a new typing EDU.
	pub async fn typing_users_for_user(
		&self,
		room_id: &RoomId,
		sender_user: &UserId,
	) -> Result<Vec<OwnedUserId>> {
		let user_ids: Vec<_> = self
			.state
			.read()
			.await
			.users(room_id)
			.into_iter()
			.stream()
			.filter_map(async |typing_user_id| {
				self.services
					.users
					.user_is_ignored(&typing_user_id, sender_user)
					.await
					.eq(&false)
					.then_some(typing_user_id)
			})
			.collect()
			.await;

		Ok(user_ids)
	}

	async fn federation_send(&self, room_id: &RoomId, user_id: &UserId, typing: bool) -> Result {
		debug_assert!(
			self.services.globals.user_is_local(user_id),
			"tried to broadcast typing status of remote user",
		);

		if !self.server.config.allow_outgoing_typing {
			return Ok(());
		}

		let content = TypingContent::new(room_id.to_owned(), user_id.to_owned(), typing);
		let edu = Edu::Typing(content);

		let mut buf = EduBuf::new();
		serde_json::to_writer(&mut buf, &edu).expect("Serialized Edu::Typing");

		self.services
			.sending
			.send_edu_room(room_id, buf)
			.await?;

		Ok(())
	}

	pub fn subscribe(&self) -> broadcast::Receiver<OwnedRoomId> {
		self.typing_update_sender.subscribe()
	}

	fn send_timer(&self, timer: TypingTimer) {
		if self.timer_channel.0.send(timer).is_err() {
			// The worker closes the channel before exiting. During shutdown no
			// timer is needed; if it stopped early, sync's lazy maintenance still
			// prevents expired state from being returned.
			if self.server.is_running() {
				debug_error!("typing timer worker stopped while the server is running");
			} else {
				trace!("typing timer worker is stopped");
			}
		}
	}

	async fn transition_state<T>(
		&self,
		transition: impl FnOnce(&mut TypingState) -> StateTransition<T>,
	) -> T {
		// Queue timer commands before releasing the state lock. This serializes
		// their order with concurrent state changes; generations additionally
		// protect against timer futures which completed before cancellation.
		let mut state = self.state.write().await;
		let StateTransition { output, timers } = transition(&mut state);
		for timer in timers {
			self.send_timer(timer);
		}

		output
	}
}
