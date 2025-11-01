mod data;
mod presence;

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{
	Stream, StreamExt, TryFutureExt,
	future::{OptionFuture, try_join},
	stream::FuturesUnordered,
};
use loole::{Receiver, Sender};
use ruma::{
	DeviceId, OwnedUserId, UInt, UserId, events::presence::PresenceEvent, presence::PresenceState,
};
use tokio::{sync::RwLock, time::sleep};
use tuwunel_core::{
	Error, Result, checked, debug, debug_warn, error, result::LogErr, trace,
	utils::future::OptionExt,
};

use self::{data::Data, presence::Presence};

pub struct Service {
	timer_channel: (Sender<TimerType>, Receiver<TimerType>),
	timeout_remote_users: bool,
	idle_timeout: u64,
	offline_timeout: u64,
	db: Data,
	services: Arc<crate::services::OnceServices>,
	last_sync_seen: RwLock<HashMap<OwnedUserId, u64>>,
}

type TimerType = (OwnedUserId, Duration);

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;
		let idle_timeout_s = config.presence_idle_timeout_s;
		let offline_timeout_s = config.presence_offline_timeout_s;
		Ok(Arc::new(Self {
			timer_channel: loole::unbounded(),
			timeout_remote_users: config.presence_timeout_remote_users,
			idle_timeout: checked!(idle_timeout_s * 1_000)?,
			offline_timeout: checked!(offline_timeout_s * 1_000)?,
			db: Data::new(args),
			services: args.services.clone(),
			last_sync_seen: RwLock::new(HashMap::new()),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		// reset dormant online/away statuses to offline, and set the server user as
		// online
		self.unset_all_presence().await;
		_ = self
			.maybe_ping_presence(&self.services.globals.server_user, None, &PresenceState::Online)
			.await;

		let receiver = self.timer_channel.1.clone();

		let mut presence_timers = FuturesUnordered::new();
		while !receiver.is_closed() {
			tokio::select! {
				Some(user_id) = presence_timers.next() => {
					self.process_presence_timer(&user_id).await.log_err().ok();
				},
				event = receiver.recv_async() => match event {
					Err(_) => break,
					Ok((user_id, timeout)) => {
						debug!("Adding timer {}: {user_id} timeout:{timeout:?}", presence_timers.len());
						presence_timers.push(presence_timer(user_id, timeout));
					},
				},
			}
		}

		Ok(())
	}

	async fn interrupt(&self) {
		// set the server user as offline
		_ = self
			.maybe_ping_presence(
				&self.services.globals.server_user,
				None,
				&PresenceState::Offline,
			)
			.await;

		let (timer_sender, _) = &self.timer_channel;
		if !timer_sender.is_closed() {
			timer_sender.close();
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// record that a user has just successfully completed a /sync (or
	/// equivalent activity)
	pub async fn note_sync(&self, user_id: &UserId) {
		if !self.services.config.suppress_push_when_active {
			return;
		}

		let now = tuwunel_core::utils::millis_since_unix_epoch();
		self.last_sync_seen
			.write()
			.await
			.insert(user_id.to_owned(), now);
	}

	/// Returns milliseconds since last observed sync for user (if any)
	pub async fn last_sync_gap_ms(&self, user_id: &UserId) -> Option<u64> {
		let now = tuwunel_core::utils::millis_since_unix_epoch();
		self.last_sync_seen
			.read()
			.await
			.get(user_id)
			.map(|ts| now.saturating_sub(*ts))
	}

	/// Returns the latest presence event for the given user.
	pub async fn get_presence(&self, user_id: &UserId) -> Result<PresenceEvent> {
		self.db
			.get_presence(user_id)
			.map_ok(|(_, presence)| presence)
			.await
	}

	/// Pings the presence of the given user, setting the specified state. When
	/// device_id is supplied.
	pub async fn maybe_ping_presence(
		&self,
		user_id: &UserId,
		device_id: Option<&DeviceId>,
		new_state: &PresenceState,
	) -> Result {
		const REFRESH_TIMEOUT: u64 = 60 * 1000;

		if !self.services.server.config.allow_local_presence || self.services.db.is_read_only() {
			return Ok(());
		}

		let last_presence = self.db.get_presence(user_id).await;
		let state_changed = match last_presence {
			| Err(_) => true,
			| Ok((_, ref presence)) => presence.content.presence != *new_state,
		};

		let last_last_active_ago = match last_presence {
			| Err(_) => 0_u64,
			| Ok((_, ref presence)) => presence
				.content
				.last_active_ago
				.unwrap_or_default()
				.into(),
		};

		if !state_changed && last_last_active_ago < REFRESH_TIMEOUT {
			return Ok(());
		}

		let update_device_seen: OptionFuture<_> = device_id
			.map(|device_id| {
				self.services
					.users
					.update_device_last_seen(user_id, device_id, None)
			})
			.into();

		let status_msg = match last_presence {
			| Ok((_, ref presence)) => presence.content.status_msg.clone(),
			| Err(_) => Some(String::new()),
		};

		let last_active_ago = UInt::new(0);
		let currently_active = *new_state == PresenceState::Online;
		let set_presence = self.set_presence(
			user_id,
			new_state,
			Some(currently_active),
			last_active_ago,
			status_msg,
		);

		try_join(set_presence, update_device_seen.unwrap_or(Ok(())))
			.map_ok(|_| ())
			.await
	}

	/// Adds a presence event which will be saved until a new event replaces it.
	pub async fn set_presence(
		&self,
		user_id: &UserId,
		state: &PresenceState,
		currently_active: Option<bool>,
		last_active_ago: Option<UInt>,
		status_msg: Option<String>,
	) -> Result {
		let presence_state = match state.as_str() {
			| "" => &PresenceState::Offline, // default an empty string to 'offline'
			| &_ => state,
		};

		self.db
			.set_presence(user_id, presence_state, currently_active, last_active_ago, status_msg)
			.await?;

		if (self.timeout_remote_users || self.services.globals.user_is_local(user_id))
			&& user_id != self.services.globals.server_user
		{
			let timeout = match presence_state {
				| PresenceState::Online =>
					self.services
						.server
						.config
						.presence_idle_timeout_s,
				| _ =>
					self.services
						.server
						.config
						.presence_offline_timeout_s,
			};

			self.timer_channel
				.0
				.send((user_id.to_owned(), Duration::from_secs(timeout)))
				.map_err(|e| {
					error!("Failed to add presence timer: {}", e);
					Error::bad_database("Failed to add presence timer")
				})?;
		}

		Ok(())
	}

	/// Removes the presence record for the given user from the database.
	///
	/// TODO: Why is this not used?
	#[allow(dead_code)]
	pub async fn remove_presence(&self, user_id: &UserId) {
		self.db.remove_presence(user_id).await;
	}

	// Unset online/unavailable presence to offline on startup
	async fn unset_all_presence(&self) {
		if !self.services.server.config.allow_local_presence || self.services.db.is_read_only() {
			return;
		}

		let _cork = self.services.db.cork();

		for user_id in &self
			.services
			.users
			.list_local_users()
			.map(UserId::to_owned)
			.collect::<Vec<_>>()
			.await
		{
			let presence = self.db.get_presence(user_id).await;

			let presence = match presence {
				| Ok((_, ref presence)) => &presence.content,
				| _ => continue,
			};

			if !matches!(
				presence.presence,
				PresenceState::Unavailable | PresenceState::Online | PresenceState::Busy
			) {
				trace!(?user_id, ?presence, "Skipping user");
				continue;
			}

			trace!(?user_id, ?presence, "Resetting presence to offline");

			_ = self
				.set_presence(
					user_id,
					&PresenceState::Offline,
					Some(false),
					presence.last_active_ago,
					presence.status_msg.clone(),
				)
				.await
				.inspect_err(|e| {
					debug_warn!(
						?presence,
						"{user_id} has invalid presence in database and failed to reset it to \
						 offline: {e}"
					);
				});
		}
	}

	/// Returns the most recent presence updates that happened after the event
	/// with id `since`.
	pub fn presence_since(
		&self,
		since: u64,
		to: Option<u64>,
	) -> impl Stream<Item = (&UserId, u64, &[u8])> + Send + '_ {
		self.db.presence_since(since, to)
	}

	#[inline]
	pub async fn from_json_bytes_to_event(
		&self,
		bytes: &[u8],
		user_id: &UserId,
	) -> Result<PresenceEvent> {
		let presence = Presence::from_json_bytes(bytes)?;
		let event = presence
			.to_presence_event(user_id, &self.services.users)
			.await;

		Ok(event)
	}

	async fn process_presence_timer(&self, user_id: &OwnedUserId) -> Result {
		let mut presence_state = PresenceState::Offline;
		let mut last_active_ago = None;
		let mut status_msg = None;

		let presence_event = self.get_presence(user_id).await;

		if let Ok(presence_event) = presence_event {
			presence_state = presence_event.content.presence;
			last_active_ago = presence_event.content.last_active_ago;
			status_msg = presence_event.content.status_msg;
		}

		let new_state = match (&presence_state, last_active_ago.map(u64::from)) {
			| (PresenceState::Online, Some(ago)) if ago >= self.idle_timeout =>
				Some(PresenceState::Unavailable),
			| (PresenceState::Unavailable, Some(ago)) if ago >= self.offline_timeout =>
				Some(PresenceState::Offline),
			| _ => None,
		};

		debug!(
			"Processed presence timer for user '{user_id}': Old state = {presence_state}, New \
			 state = {new_state:?}"
		);

		if let Some(new_state) = new_state {
			self.set_presence(user_id, &new_state, Some(false), last_active_ago, status_msg)
				.await?;
		}

		Ok(())
	}
}

async fn presence_timer(user_id: OwnedUserId, timeout: Duration) -> OwnedUserId {
	sleep(timeout).await;

	user_id
}
