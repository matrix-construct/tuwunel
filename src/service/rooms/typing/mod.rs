use std::{collections::BTreeMap, sync::Arc};

use futures::StreamExt;
use ruma::{
	OwnedRoomId, OwnedUserId, RoomId, UserId,
	api::federation::transactions::edu::{Edu, TypingContent},
	events::SyncEphemeralRoomEvent,
};
use tokio::sync::{RwLock, broadcast};
use tuwunel_core::{
	Result, Server, debug_info, trace,
	utils::{self, IterStream},
};

use crate::{Dep, globals, sending, sending::EduBuf, users};

pub struct Service {
	server: Arc<Server>,
	services: Services,
	/// u64 is unix timestamp of timeout
	pub typing: RwLock<BTreeMap<OwnedRoomId, BTreeMap<OwnedUserId, u64>>>,
	/// timestamp of the last change to typing users
	pub last_typing_update: RwLock<BTreeMap<OwnedRoomId, u64>>,
	pub typing_update_sender: broadcast::Sender<OwnedRoomId>,
}

struct Services {
	globals: Dep<globals::Service>,
	sending: Dep<sending::Service>,
	users: Dep<users::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			server: args.server.clone(),
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				sending: args.depend::<sending::Service>("sending"),
				users: args.depend::<users::Service>("users"),
			},
			typing: RwLock::new(BTreeMap::new()),
			last_typing_update: RwLock::new(BTreeMap::new()),
			typing_update_sender: broadcast::channel(100).0,
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

impl Service {
	/// Sets a user as typing until the timeout timestamp is reached or
	/// roomtyping_remove is called.
	pub async fn typing_add(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		timeout: u64,
	) -> Result<()> {
		debug_info!("typing started {user_id:?} in {room_id:?} timeout:{timeout:?}");
		// update clients
		self.typing
			.write()
			.await
			.entry(room_id.to_owned())
			.or_default()
			.insert(user_id.to_owned(), timeout);

		self.last_typing_update
			.write()
			.await
			.insert(room_id.to_owned(), self.services.globals.next_count()?);

		if self
			.typing_update_sender
			.send(room_id.to_owned())
			.is_err()
		{
			trace!("receiver found what it was looking for and is no longer interested");
		}

		// update federation
		if self.services.globals.user_is_local(user_id) {
			self.federation_send(room_id, user_id, true)
				.await?;
		}

		Ok(())
	}

	/// Removes a user from typing before the timeout is reached.
	pub async fn typing_remove(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
		debug_info!("typing stopped {user_id:?} in {room_id:?}");
		// update clients
		self.typing
			.write()
			.await
			.entry(room_id.to_owned())
			.or_default()
			.remove(user_id);

		self.last_typing_update
			.write()
			.await
			.insert(room_id.to_owned(), self.services.globals.next_count()?);

		if self
			.typing_update_sender
			.send(room_id.to_owned())
			.is_err()
		{
			trace!("receiver found what it was looking for and is no longer interested");
		}

		// update federation
		if self.services.globals.user_is_local(user_id) {
			self.federation_send(room_id, user_id, false)
				.await?;
		}

		Ok(())
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
	async fn typings_maintain(&self, room_id: &RoomId) -> Result<()> {
		let current_timestamp = utils::millis_since_unix_epoch();
		let mut removable = Vec::new();

		{
			let typing = self.typing.read().await;
			let Some(room) = typing.get(room_id) else {
				return Ok(());
			};

			for (user, timeout) in room {
				if *timeout < current_timestamp {
					removable.push(user.clone());
				}
			}
		};

		if !removable.is_empty() {
			let typing = &mut self.typing.write().await;
			let room = typing.entry(room_id.to_owned()).or_default();
			for user in &removable {
				debug_info!("typing timeout {user:?} in {room_id:?}");
				room.remove(user);
			}

			// update clients
			self.last_typing_update
				.write()
				.await
				.insert(room_id.to_owned(), self.services.globals.next_count()?);

			if self
				.typing_update_sender
				.send(room_id.to_owned())
				.is_err()
			{
				trace!("receiver found what it was looking for and is no longer interested");
			}

			// update federation
			for user in &removable {
				if self.services.globals.user_is_local(user) {
					self.federation_send(room_id, user, false).await?;
				}
			}
		}

		Ok(())
	}

	/// Returns the count of the last typing update in this room.
	pub async fn last_typing_update(&self, room_id: &RoomId) -> Result<u64> {
		self.typings_maintain(room_id).await?;
		Ok(self
			.last_typing_update
			.read()
			.await
			.get(room_id)
			.copied()
			.unwrap_or(0))
	}

	/// Returns a new typing EDU.
	pub async fn typings_all(
		&self,
		room_id: &RoomId,
		sender_user: &UserId,
	) -> Result<SyncEphemeralRoomEvent<ruma::events::typing::TypingEventContent>> {
		let room_typing_indicators = self.typing.read().await.get(room_id).cloned();

		let Some(typing_indicators) = room_typing_indicators else {
			return Ok(SyncEphemeralRoomEvent {
				content: ruma::events::typing::TypingEventContent { user_ids: Vec::new() },
			});
		};

		let user_ids: Vec<_> = typing_indicators
			.into_keys()
			.stream()
			.filter_map(|typing_user_id| async move {
				(!self
					.services
					.users
					.user_is_ignored(&typing_user_id, sender_user)
					.await)
					.then_some(typing_user_id)
			})
			.collect()
			.await;

		Ok(SyncEphemeralRoomEvent {
			content: ruma::events::typing::TypingEventContent { user_ids },
		})
	}

	async fn federation_send(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		typing: bool,
	) -> Result<()> {
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
