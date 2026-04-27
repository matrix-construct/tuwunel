use std::{collections::BTreeMap, sync::Arc};

use futures::{FutureExt, StreamExt, TryStreamExt, future::try_join};
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
	Result, Server, debug_info, trace,
	utils::{self, BoolExt, IterStream},
};

use crate::sending::EduBuf;

pub struct Service {
	server: Arc<Server>,
	services: Arc<crate::services::OnceServices>,
	/// u64 is unix timestamp of timeout
	pub typing: RwLock<BTreeMap<OwnedRoomId, BTreeMap<OwnedUserId, u64>>>,
	/// timestamp of the last change to typing users
	pub last_typing_update: RwLock<BTreeMap<OwnedRoomId, u64>>,
	pub typing_update_sender: broadcast::Sender<OwnedRoomId>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			server: args.server.clone(),
			services: args.services.clone(),
			typing: RwLock::new(BTreeMap::new()),
			last_typing_update: RwLock::new(BTreeMap::new()),
			typing_update_sender: broadcast::channel(100).0,
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Sets a user as typing until the timeout timestamp is reached or
	/// roomtyping_remove is called.
	pub async fn typing_add(&self, user_id: &UserId, room_id: &RoomId, timeout: u64) -> Result {
		debug_info!("typing started {user_id:?} in {room_id:?} timeout:{timeout:?}");

		// update clients
		self.typing
			.write()
			.await
			.entry(room_id.to_owned())
			.or_default()
			.insert(user_id.to_owned(), timeout);

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
	pub async fn typing_remove(&self, user_id: &UserId, room_id: &RoomId) -> Result {
		debug_info!("typing stopped {user_id:?} in {room_id:?}");

		// update clients
		self.typing
			.write()
			.await
			.entry(room_id.to_owned())
			.or_default()
			.remove(user_id);

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

	pub async fn wait_for_update(&self, room_id: &RoomId) {
		let mut receiver = self.typing_update_sender.subscribe();
		while let Ok(next) = receiver.recv().await {
			if next == room_id {
				break;
			}
		}
	}

	/// Makes sure that typing events with old timestamps get removed.
	async fn typings_maintain(&self, room_id: &RoomId) -> Result {
		let current_timestamp = utils::millis_since_unix_epoch();
		let mut removable = Vec::new();

		let typing = self.typing.read().await;
		let Some(room) = typing.get(room_id) else {
			return Ok(());
		};

		for (user, timeout) in room {
			if *timeout < current_timestamp {
				removable.push(user.clone());
			}
		}

		drop(typing);

		if removable.is_empty() {
			return Ok(());
		}

		let mut typing = self.typing.write().await;
		let room = typing.entry(room_id.to_owned()).or_default();
		for user in &removable {
			debug_info!("typing timeout {user:?} in {room_id:?}");
			room.remove(user);
		}

		drop(typing);

		// update clients
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

		// update appservices
		let appservice_send = self.appservice_send(room_id);

		// update federation
		let federation_sends = removable
			.iter()
			.filter(|user_id| self.services.globals.user_is_local(user_id))
			.try_stream()
			.try_for_each(|user_id| self.federation_send(room_id, user_id, false));

		try_join(appservice_send, federation_sends)
			.boxed()
			.await
			.map(|_| ())
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
		let room_typing_indicators = self.typing.read().await.get(room_id).cloned();

		let Some(typing_indicators) = room_typing_indicators else {
			return TypingEventContent { user_ids: Vec::new() };
		};

		TypingEventContent {
			user_ids: typing_indicators.into_keys().collect(),
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
		let room_typing_indicators = self.typing.read().await.get(room_id).cloned();

		let Some(typing_indicators) = room_typing_indicators else {
			return Ok(Vec::new());
		};

		let user_ids: Vec<_> = typing_indicators
			.into_keys()
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
}
