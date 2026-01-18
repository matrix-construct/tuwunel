mod aggregate;
mod data;
mod presence;

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{
	Stream, StreamExt, TryFutureExt,
	future::{AbortHandle, Abortable, try_join},
	stream::FuturesUnordered,
};
use loole::{Receiver, Sender};
use ruma::{
	DeviceId, OwnedUserId, UInt, UserId, events::presence::PresenceEvent, presence::PresenceState,
};
use tokio::{sync::RwLock, time::sleep};
use tuwunel_core::{
	Error, Result, checked, debug, debug_warn, error,
	result::LogErr,
	trace,
	utils::{future::OptionFutureExt, option::OptionExt},
};

use self::{aggregate::PresenceAggregator, data::Data, presence::Presence};

pub struct Service {
	timer_channel: (Sender<TimerType>, Receiver<TimerType>),
	timeout_remote_users: bool,
	idle_timeout: u64,
	offline_timeout: u64,
	db: Data,
	services: Arc<crate::services::OnceServices>,
	last_sync_seen: RwLock<HashMap<OwnedUserId, u64>>,
	device_presence: PresenceAggregator,
}

type TimerType = (OwnedUserId, Duration, u64);
type TimerFired = (OwnedUserId, u64);

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
			device_presence: PresenceAggregator::new(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		// reset dormant online/away statuses to offline, and set the server user as
		// online
		self.unset_all_presence().await;
		self.device_presence.clear().await;
		_ = self
			.maybe_ping_presence(&self.services.globals.server_user, None, &PresenceState::Online)
			.await;

		let receiver = self.timer_channel.1.clone();

		let mut presence_timers: FuturesUnordered<_> = FuturesUnordered::new();
		let mut timer_handles: HashMap<OwnedUserId, (u64, AbortHandle)> = HashMap::new();
		while !receiver.is_closed() {
			tokio::select! {
				Some(result) = presence_timers.next() => {
					let Ok((user_id, count)) = result else {
						continue;
					};

					if let Some((current_count, _)) = timer_handles.get(&user_id) {
						if *current_count != count {
							trace!(?user_id, count, current_count, "Skipping stale presence timer");
							continue;
						}
					}

					timer_handles.remove(&user_id);
					self.process_presence_timer(&user_id, count).await.log_err().ok();
				},
				event = receiver.recv_async() => match event {
					Err(_) => break,
					Ok((user_id, timeout, count)) => {
						debug!(
							"Adding timer {}: {user_id} timeout:{timeout:?} count:{count}",
							presence_timers.len()
						);
						if let Some((_, handle)) = timer_handles.remove(&user_id) {
							handle.abort();
						}

						let (handle, reg) = AbortHandle::new_pair();
						presence_timers.push(Abortable::new(
							presence_timer(user_id.clone(), timeout, count),
							reg,
						));
						timer_handles.insert(user_id, (count, handle));
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
	fn device_key(device_id: Option<&DeviceId>, is_remote: bool) -> aggregate::DeviceKey {
		if is_remote {
			return aggregate::DeviceKey::Remote;
		}

		match device_id {
			| Some(device_id) => aggregate::DeviceKey::Device(device_id.to_owned()),
			| None => aggregate::DeviceKey::UnknownLocal,
		}
	}

	fn schedule_presence_timer(
		&self,
		user_id: &UserId,
		presence_state: &PresenceState,
		count: u64,
	) -> Result {
		if !(self.timeout_remote_users || self.services.globals.user_is_local(user_id))
			|| user_id == self.services.globals.server_user
		{
			return Ok(());
		}

		let timeout = match presence_state {
			| PresenceState::Online => self.services.server.config.presence_idle_timeout_s,
			| _ => self.services.server.config.presence_offline_timeout_s,
		};

		self.timer_channel
			.0
			.send((user_id.to_owned(), Duration::from_secs(timeout), count))
			.map_err(|e| {
				error!("Failed to add presence timer: {}", e);
				Error::bad_database("Failed to add presence timer")
			})
	}

	async fn apply_device_presence_update(
		&self,
		user_id: &UserId,
		device_key: aggregate::DeviceKey,
		state: &PresenceState,
		currently_active: Option<bool>,
		last_active_ago: Option<UInt>,
		status_msg: Option<String>,
		reason: PresenceUpdateReason,
		refresh_window_ms: Option<u64>,
	) -> Result {
		let now = tuwunel_core::utils::millis_since_unix_epoch();
		debug!(
			?user_id,
			?device_key,
			?state,
			currently_active,
			last_active_ago = last_active_ago.map(u64::from),
			?reason,
			"Presence update received"
		);
		self.device_presence
			.update(
				user_id,
				device_key,
				state,
				currently_active,
				last_active_ago,
				status_msg,
				now,
			)
			.await;

		let aggregated = self
			.device_presence
			.aggregate(user_id, now, self.idle_timeout, self.offline_timeout)
			.await;
		debug!(
			?user_id,
			agg_state = ?aggregated.state,
			agg_currently_active = aggregated.currently_active,
			agg_last_active_ts = aggregated.last_active_ts,
			agg_device_count = aggregated.device_count,
			"Presence aggregate computed"
		);

		let last_presence = self.db.get_presence(user_id).await;
		let (last_count, last_event) = match last_presence {
			| Ok((count, event)) => (Some(count), Some(event)),
			| Err(_) => (None, None),
		};

		let state_changed = match &last_event {
			| Some(event) => event.content.presence != aggregated.state,
			| None => true,
		};

		if !state_changed {
			if let (Some(refresh_ms), Some(event), Some(count)) =
				(refresh_window_ms, &last_event, last_count)
			{
				let last_last_active_ago: u64 = event
					.content
					.last_active_ago
					.unwrap_or_default()
					.into();
				if last_last_active_ago < refresh_ms {
					self.schedule_presence_timer(user_id, &event.content.presence, count)
						.log_err()
						.ok();
					debug!(
						?user_id,
						?state,
						last_last_active_ago,
						"Skipping presence update: refresh window (timer rescheduled)"
					);
					return Ok(());
				}
			}
		}

		let fallback_status = last_event
			.and_then(|event| event.content.status_msg)
			.filter(|msg| !msg.is_empty());
		let status_msg = aggregated.status_msg.or(fallback_status);
		let last_active_ago =
			Some(UInt::new_saturating(now.saturating_sub(aggregated.last_active_ts)));

		self.set_presence(
			user_id,
			&aggregated.state,
			Some(aggregated.currently_active),
			last_active_ago,
			status_msg,
			reason,
		)
		.await
	}

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
		const REFRESH_TIMEOUT: u64 = 30 * 1000;

		if !self.services.server.config.allow_local_presence || self.services.db.is_read_only() {
			debug!(
				?user_id,
				?new_state,
				allow_local_presence = self.services.server.config.allow_local_presence,
				read_only = self.services.db.is_read_only(),
				"Skipping presence ping"
			);
			return Ok(());
		}

		let update_device_seen = device_id.map_async(|device_id| {
			self.services
				.users
				.update_device_last_seen(user_id, device_id, None)
		});

		let currently_active = *new_state == PresenceState::Online;
		let set_presence = self.apply_device_presence_update(
			user_id,
			Self::device_key(device_id, false),
			new_state,
			Some(currently_active),
			UInt::new(0),
			None,
			PresenceUpdateReason::Ping,
			Some(REFRESH_TIMEOUT),
		);

		debug!(
			?user_id,
			?new_state,
			currently_active,
			"Presence ping accepted"
		);

		try_join(set_presence, update_device_seen.unwrap_or(Ok(())))
			.map_ok(|_| ())
			.await
	}

	/// Applies an explicit presence update for a local device.
	pub async fn set_presence_for_device(
		&self,
		user_id: &UserId,
		device_id: Option<&DeviceId>,
		state: &PresenceState,
		status_msg: Option<String>,
		reason: PresenceUpdateReason,
	) -> Result {
		let currently_active = *state == PresenceState::Online;
		self.apply_device_presence_update(
			user_id,
			Self::device_key(device_id, false),
			state,
			Some(currently_active),
			None,
			status_msg,
			reason,
			None,
		)
		.await
	}

	/// Applies a presence update received over federation.
	pub async fn set_presence_from_federation(
		&self,
		user_id: &UserId,
		state: &PresenceState,
		currently_active: bool,
		last_active_ago: UInt,
		status_msg: Option<String>,
		reason: PresenceUpdateReason,
	) -> Result {
		self.apply_device_presence_update(
			user_id,
			Self::device_key(None, true),
			state,
			Some(currently_active),
			Some(last_active_ago),
			status_msg,
			reason,
			None,
		)
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

		let count = self
			.db
			.set_presence(user_id, presence_state, currently_active, last_active_ago, status_msg)
			.await?;

		if let Some(count) = count {
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

				let timeout_kind = match presence_state {
					| PresenceState::Online => "idle_timeout_s",
					| _ => "offline_timeout_s",
				};

				debug!(
					?user_id,
					?presence_state,
					currently_active,
					last_active_ago = last_active_ago.map(u64::from),
					status_msg = status_msg_log.as_deref(),
					count,
					timeout_s = timeout,
					timeout_kind,
					timeout_remote_users = self.timeout_remote_users,
					is_local,
					is_server_user,
					"Scheduling presence timer"
				);

				self.schedule_presence_timer(user_id, presence_state, count)?;
			} else {
				debug!(
					?user_id,
					?presence_state,
					currently_active,
					last_active_ago = last_active_ago.map(u64::from),
					status_msg = status_msg_log.as_deref(),
					count,
					timeout_remote_users = self.timeout_remote_users,
					is_local,
					is_server_user,
					"Presence timer not scheduled"
				);
			}
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

	async fn process_presence_timer(&self, user_id: &OwnedUserId, expected_count: u64) -> Result {
		let (current_count, presence) = match self.db.get_presence_raw(user_id).await {
			| Ok(presence) => presence,
			| Err(_) => return Ok(()),
		};

		if current_count != expected_count {
			trace!(
				?user_id,
				expected_count,
				current_count,
				"Skipping stale presence timer"
			);
			return Ok(());
		}

		let presence_state = presence.state().clone();
		let now = tuwunel_core::utils::millis_since_unix_epoch();
		let aggregated = self
			.device_presence
			.aggregate(user_id, now, self.idle_timeout, self.offline_timeout)
			.await;

		if aggregated.device_count == 0 {
			let last_active_ago =
				Some(UInt::new_saturating(now.saturating_sub(presence.last_active_ts())));
			let status_msg = presence.status_msg();
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
				let reason = match new_state {
					| PresenceState::Unavailable => PresenceUpdateReason::TimerIdle,
					| PresenceState::Offline => PresenceUpdateReason::TimerOffline,
					| _ => PresenceUpdateReason::Ping,
				};

				self.set_presence(
					user_id,
					&new_state,
					Some(false),
					last_active_ago,
					status_msg,
					reason,
				)
				.await?;
			}

			return Ok(());
		}

		if aggregated.state == presence_state {
			self.schedule_presence_timer(user_id, &presence_state, current_count)
				.log_err()
				.ok();
			return Ok(());
		}

		let reason = match aggregated.state {
			| PresenceState::Unavailable => PresenceUpdateReason::TimerIdle,
			| PresenceState::Offline => PresenceUpdateReason::TimerOffline,
			| _ => PresenceUpdateReason::Ping,
		};

		let status_msg = aggregated.status_msg.or_else(|| presence.status_msg());
		let last_active_ago =
			Some(UInt::new_saturating(now.saturating_sub(aggregated.last_active_ts)));

		self.set_presence(
			user_id,
			&aggregated.state,
			Some(aggregated.currently_active),
			last_active_ago,
			status_msg,
			reason,
		)
		.await?;

		Ok(())
	}
}

async fn presence_timer(user_id: OwnedUserId, timeout: Duration, count: u64) -> TimerFired {
	sleep(timeout).await;

	(user_id, count)
}
