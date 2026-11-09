use std::{collections::BTreeMap, sync::Arc};

use futures::{FutureExt, StreamExt, TryStreamExt, future::try_join, stream::unfold};
use ruma::{
	OwnedRoomId, OwnedUserId, RoomId, UserId,
	api::{
		appservice::event::push_events::v1::EphemeralData,
		federation::transactions::edu::{Edu, TypingContent},
	},
	events::{
		EphemeralRoomEvent, GlobalAccountDataEventType, ignored_user_list::IgnoredUserListEvent,
		typing::TypingEventContent,
	},
};
use tokio::sync::{
	RwLock,
	broadcast::{self, error::RecvError},
};
use tuwunel_core::{
	Result, Server,
	debug::INFO_SPAN_LEVEL,
	debug_info, trace,
	utils::{BoolExt, IterStream, millis_since_unix_epoch},
};

use crate::sending::EduBuf;

pub struct Service {
	server: Arc<Server>,
	services: Arc<crate::services::OnceServices>,
	typing: RwLock<BTreeMap<OwnedRoomId, RoomTyping>>,
	pub typing_update_sender: broadcast::Sender<OwnedRoomId>,
}

#[derive(Default)]
struct RoomTyping {
	// Unix epoch millisecond timestamp when each typing indicator expires.
	users: BTreeMap<OwnedUserId, u64>,
	// Global stream position of the last change to this room. The count permit must
	// retire only after releasing the typing state lock.
	update: u64,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			server: args.server.clone(),
			services: args.services.clone(),
			typing: RwLock::new(BTreeMap::new()),
			typing_update_sender: broadcast::channel(100).0,
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Sets a user as typing until the timeout timestamp is reached or
	/// roomtyping_remove is called.
	#[tracing::instrument(
		name = "typing_start"
		level = INFO_SPAN_LEVEL,
		skip_all,
		 fields(
			%room_id,
			%user_id,
			%timeout,
		)
	)]
	pub async fn typing_add(&self, user_id: &UserId, room_id: &RoomId, timeout: u64) -> Result {
		debug_info!("typing started {user_id:?} in {room_id:?} timeout:{timeout:?}");

		// update clients
		let mut typing = self.typing.write().await;
		let room = typing.entry(room_id.to_owned()).or_default();
		room.users.insert(user_id.to_owned(), timeout);

		let count = self.services.globals.next_count();

		room.update = *count;

		drop(typing);
		drop(count);

		if self
			.typing_update_sender
			.send(room_id.to_owned())
			.is_err()
		{
			trace!("receiver found what it was looking for and is no longer interested");
		}

		// update appservices
		let appservice_send = self.appservice_send(room_id);

		// update federation
		let federation_send = self
			.services
			.globals
			.user_is_local(user_id)
			.then_async(|| self.federation_send(room_id, user_id, true))
			.map(Option::transpose);

		try_join(appservice_send, federation_send)
			.await
			.map(|_| ())
	}

	/// Removes a user from typing before the timeout is reached.
	#[tracing::instrument(
		name = "typing_stop"
		level = INFO_SPAN_LEVEL,
		skip_all,
		 fields(
			%room_id,
			%user_id,
		)
	)]
	pub async fn typing_remove(&self, user_id: &UserId, room_id: &RoomId) -> Result {
		debug_info!("typing stopped {user_id:?} in {room_id:?}");

		// update clients
		let mut typing = self.typing.write().await;
		let room = typing.entry(room_id.to_owned()).or_default();
		room.users.remove(user_id);

		let count = self.services.globals.next_count();

		room.update = *count;

		drop(typing);
		drop(count);

		if self
			.typing_update_sender
			.send(room_id.to_owned())
			.is_err()
		{
			trace!("receiver found what it was looking for and is no longer interested");
		}

		// update appservices
		let appservice_send = self.appservice_send(room_id);

		// update federation
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

	/// Returns a future that resolves on the next typing update in this room.
	///
	/// The receiver subscribes before this returns, so the update is seen however
	/// late the future is first polled. A lagged receiver has lost the messages it
	/// would have matched, so it asks the room instead. Callers pass a retired
	/// count as `since` and run `wait_pending` after registering, so an update at
	/// or below it needs no wake.
	pub fn wait_for_update(
		&self,
		room_id: OwnedRoomId,
		since: u64,
	) -> impl Future<Output = ()> + Send + '_ {
		let receiver = self.typing_update_sender.subscribe();

		// Not an `async fn`, which would defer the subscribe to the first poll.
		// The block owns `room_id`: `any` rejects an async closure owning it.
		async move {
			unfold(receiver, async |mut receiver| Some((receiver.recv().await, receiver)))
				.any(async |update| match update {
					| Ok(next) => next == room_id,
					| Err(RecvError::Lagged(_)) => self.lag_wakes(&room_id, since).await,
					| Err(RecvError::Closed) => true,
				})
				.await;
		}
	}

	/// Returns whether a receiver that lagged should wake for this room.
	///
	/// It should for an update past `since`, and for a typist past their timeout,
	/// whom only a pass removes. This reads and leaves the removal to that pass: a
	/// sibling's wake can drop the watcher at any await, and expiry removes typists
	/// before it tells federation, so a drop there would lose the stop for good.
	async fn lag_wakes(&self, room_id: &RoomId, since: u64) -> bool {
		let now = millis_since_unix_epoch();

		self.typing
			.read()
			.map(|typing| {
				typing
					.get(room_id)
					.is_some_and(|room| room.update > since || room.has_expired(now))
			})
			.await
	}

	/// Makes sure that typing events with old timestamps get removed.
	async fn typings_maintain(&self, room_id: &RoomId) -> Result {
		let current_timestamp = millis_since_unix_epoch();
		let typing = self.typing.read().await;
		let has_expired = typing
			.get(room_id)
			.is_some_and(|room| room.has_expired(current_timestamp));

		drop(typing);

		if !has_expired {
			return Ok(());
		}

		let current_timestamp = millis_since_unix_epoch();
		let mut removable = Vec::new();
		let mut typing = self.typing.write().await;
		let Some(room) = typing.get_mut(room_id) else {
			return Ok(());
		};

		room.users.retain(|user, timeout| {
			let expired = *timeout < current_timestamp;
			if expired {
				removable.push(user.clone());
			}

			expired.is_false()
		});

		if removable.is_empty() {
			return Ok(());
		}

		// update clients
		let count = self.services.globals.next_count();

		room.update = *count;

		drop(typing);
		drop(count);

		for user in &removable {
			debug_info!("typing timeout {user:?} in {room_id:?}");
		}

		if self
			.typing_update_sender
			.send(room_id.to_owned())
			.is_err()
		{
			trace!("receiver found what it was looking for and is no longer interested");
		}

		// update appservices
		let appservice_send = self.appservice_send(room_id);

		// update federation
		let federation_sends = removable
			.iter()
			.filter(|user_id| self.services.globals.user_is_local(user_id))
			.try_stream()
			.try_for_each(|user_id| self.federation_send(room_id, user_id, false));

		try_join(appservice_send, federation_sends)
			.boxed() // erase region
			.await
			.map(|_| ())
	}

	/// Returns the count of the last typing update in this room.
	pub async fn last_typing_update(&self, room_id: &RoomId) -> Result<u64> {
		self.typings_maintain(room_id).await?;

		self.typing
			.read()
			.await
			.get(room_id)
			.map(|room| room.update)
			.map(Ok)
			.unwrap_or(Ok(0))
	}

	/// Returns the typing content with all typing users in the room.
	async fn typings_content(&self, room_id: &RoomId) -> TypingEventContent {
		let typing = self.typing.read().await;
		let user_ids = typing
			.get(room_id)
			.into_iter()
			.flat_map(|room| room.users.keys().cloned())
			.collect();

		TypingEventContent { user_ids }
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
		let typing = self.typing.read().await;
		let user_ids = typing
			.get(room_id)
			.into_iter()
			.flat_map(|room| room.users.keys().cloned())
			.collect();
		drop(typing);

		Ok(self
			.filter_typing_users(user_ids, sender_user)
			.await)
	}

	/// Returns one coherent typing update token and visible user snapshot.
	///
	/// The predicate checks the token under the same lock and can skip cloning
	/// stale users. Selected user IDs are captured before ignored users are
	/// filtered after releasing the lock.
	pub async fn typing_snapshot_for_user<Select>(
		&self,
		room_id: &RoomId,
		sender_user: &UserId,
		select: Select,
	) -> Result<Option<(u64, Vec<OwnedUserId>)>>
	where
		Select: FnOnce(u64) -> bool + Send,
	{
		self.typings_maintain(room_id).await?;

		let typing = self.typing.read().await;
		let room = typing.get(room_id);
		let update = room.map_or(0, |room| room.update);

		if !select(update) {
			return Ok(None);
		}

		let user_ids = room
			.into_iter()
			.flat_map(|room| room.users.keys().cloned())
			.collect();

		drop(typing);

		let user_ids = self
			.filter_typing_users(user_ids, sender_user)
			.await;

		Ok(Some((update, user_ids)))
	}

	async fn filter_typing_users(
		&self,
		user_ids: Vec<OwnedUserId>,
		sender_user: &UserId,
	) -> Vec<OwnedUserId> {
		if user_ids.is_empty() {
			return user_ids;
		}

		let ignored: Option<IgnoredUserListEvent> = self
			.services
			.account_data
			.get_global(sender_user, GlobalAccountDataEventType::IgnoredUserList)
			.await
			.ok();

		user_ids
			.into_iter()
			.filter(|user_id| {
				ignored.as_ref().is_none_or(|ignored| {
					!ignored
						.content
						.ignored_users
						.contains_key::<UserId>(user_id.as_ref())
				})
			})
			.collect()
	}

	async fn federation_send(&self, room_id: &RoomId, user_id: &UserId, typing: bool) -> Result {
		debug_assert!(
			self.services.globals.user_is_local(user_id),
			"tried to broadcast typing status of remote user",
		);

		if !self.server.config.typing.allow_outgoing {
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
}

impl RoomTyping {
	fn has_expired(&self, now: u64) -> bool { self.users.values().any(|timeout| *timeout < now) }
}
