mod aggregate;
mod data;
// Write/update pipeline lives in pipeline.rs.
mod pipeline;

use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{
	Stream, StreamExt, TryFutureExt,
	future::{AbortHandle, Abortable, join},
	stream::FuturesUnordered,
};
use loole::{Receiver, Sender};
use ruma::{
	DeviceId, OwnedUserId, UInt, UserId,
	events::presence::{PresenceEvent, PresenceEventContent},
	presence::PresenceState,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tuwunel_core::{
	Result, checked, debug, debug_warn, err,
	result::LogErr,
	trace,
	utils::{self, TryFutureExtExt},
};

use self::{aggregate::PresenceAggregator, data::Data};
use crate::appservice::RegistrationInfo;

#[derive(Default)]
pub struct Ping<'a> {
	pub device_id: Option<&'a DeviceId>,
	pub client_ip: Option<IpAddr>,
	pub new_state: Option<&'a PresenceState>,
	pub appservice: Option<&'a RegistrationInfo>,
}

/// Represents data required to be kept in order to implement the presence
/// specification.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(super) struct Presence {
	pub(super) state: PresenceState,
	pub(super) currently_active: bool,
	pub(super) last_active_ts: u64,
	pub(super) status_msg: Option<String>,
}

impl Presence {
	pub(super) fn from_json_bytes(bytes: &[u8]) -> Result<Self> {
		serde_json::from_slice(bytes)
			.map_err(|_| err!(Database(error!("Invalid presence data in database"))))
	}
}

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
			.maybe_ping_presence(&self.services.globals.server_user, Ping::default())
			.await;

		let receiver = self.timer_channel.1.clone();

		let mut presence_timers: FuturesUnordered<_> = FuturesUnordered::new();
		let mut timer_handles: HashMap<OwnedUserId, (u64, AbortHandle)> = HashMap::new();
		while !receiver.is_closed() && self.services.server.is_running() {
			tokio::select! {
				Some(result) = presence_timers.next() => {
					let Ok((user_id, count)) = result else {
						continue;
					};

					if let Some((current_count, _)) = timer_handles.get(&user_id)
						&& *current_count != count {
						trace!(?user_id, count, current_count, "Skipping stale presence timer");
						continue;
					}

					timer_handles.remove(&user_id);
					self.process_presence_timer(&user_id, count).await.log_err().ok();
				},
				event = receiver.recv_async() => match event {
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
							pipeline::presence_timer(user_id.clone(), timeout, count),
							reg,
						));
						timer_handles.insert(user_id, (count, handle));
					},
					_ => break,
				},
			}
		}

		// set the server user as offline
		let ping = Ping {
			new_state: Some(&PresenceState::Offline),
			..Default::default()
		};

		_ = self
			.maybe_ping_presence(&self.services.globals.server_user, ping)
			.await;

		Ok(())
	}

	async fn interrupt(&self) {
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
	pub async fn note_sync(&self, user_id: &UserId, appservice: Option<&RegistrationInfo>) {
		if appservice.is_some() || !self.services.config.suppress_push_when_active {
			return;
		}

		let now = utils::millis_since_unix_epoch();
		self.last_sync_seen
			.write()
			.await
			.insert(user_id.to_owned(), now);
	}

	/// Returns milliseconds since last observed sync for user (if any)
	pub async fn last_sync_gap_ms(&self, user_id: &UserId) -> Option<u64> {
		let now = utils::millis_since_unix_epoch();
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

	/// Removes the presence record for the given user from the database.
	///
	/// TODO: Why is this not used?
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
		let event = self.to_presence_event(presence, user_id).await;

		Ok(event)
	}

	/// Creates a PresenceEvent from available data.
	async fn to_presence_event(&self, presence: Presence, user_id: &UserId) -> PresenceEvent {
		let now = utils::millis_since_unix_epoch();
		let last_active_ago = now.saturating_sub(presence.last_active_ts);

		let avatar_url = self.services.profile.avatar_url(user_id).ok();
		let displayname = self.services.profile.displayname(user_id).ok();
		let (avatar_url, displayname) = join(avatar_url, displayname).await;

		PresenceEvent {
			sender: user_id.to_owned(),
			content: PresenceEventContent {
				presence: presence.state,
				status_msg: presence.status_msg,
				currently_active: Some(presence.currently_active),
				last_active_ago: Some(UInt::new_saturating(last_active_ago)),
				avatar_url,
				displayname,
			},
		}
	}
}
