//! Presence update pipeline.
//!
//! This module centralizes the write path for presence updates. It keeps the
//! aggregation and timer logic in one place so the public `Service` surface
//! remains small and the update flow is easy to review.

use std::time::Duration;

use futures::TryFutureExt;
use ruma::{
	DeviceId, OwnedUserId, UInt, UserId, events::presence::PresenceEvent, presence::PresenceState,
};
use tokio::time::sleep;
use tuwunel_core::{
	Error, Result, debug, error,
	result::LogErr,
	trace,
	utils::{future::OptionFutureExt, option::OptionExt},
};

use super::{Service, TimerFired, aggregate};

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
			.send((user_id.to_owned(), Duration::from_secs(timeout), count))
			.map_err(|e| {
				error!("Failed to add presence timer: {}", e);
				Error::bad_database("Failed to add presence timer")
			})
	}

	fn refresh_skip_decision(
		refresh_window_ms: Option<u64>,
		last_event: Option<&PresenceEvent>,
		last_count: Option<u64>,
	) -> Option<(u64, u64)> {
		let (Some(refresh_ms), Some(event), Some(count)) =
			(refresh_window_ms, last_event, last_count)
		else {
			return None;
		};

		let last_last_active_ago: u64 = event.content.last_active_ago?.into();

		(last_last_active_ago < refresh_ms).then_some((count, last_last_active_ago))
	}

	fn timer_is_stale(expected_count: u64, current_count: u64) -> bool {
		expected_count != current_count
	}

	#[expect(clippy::too_many_arguments)]
	async fn apply_device_presence_update(
		&self,
		user_id: &UserId,
		device_key: aggregate::DeviceKey,
		state: &PresenceState,
		currently_active: Option<bool>,
		last_active_ago: Option<UInt>,
		status_msg: Option<String>,
		refresh_window_ms: Option<u64>,
	) -> Result {
		let now = tuwunel_core::utils::millis_since_unix_epoch();
		// 1) Capture per-device presence snapshot for aggregation.
		debug!(
			?user_id,
			?device_key,
			?state,
			currently_active,
			last_active_ago = last_active_ago.map(u64::from),
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

		// 2) Compute the aggregated presence across all devices.
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

		// 3) Load the last persisted presence to decide whether to skip or merge.
		let last_presence = self.db.get_presence(user_id).await;
		let (last_count, last_event) = match last_presence {
			| Ok((count, event)) => (Some(count), Some(event)),
			| Err(_) => (None, None),
		};

		let last_state = last_event
			.as_ref()
			.map(|event| event.content.presence.clone());

		let state_changed = match &last_event {
			| Some(event) => event.content.presence != aggregated.state,
			| None => true,
		};

		// 4) For rapid pings with no state change, skip writes and reschedule.
		if !state_changed
			&& let Some((count, last_last_active_ago)) =
				Self::refresh_skip_decision(refresh_window_ms, last_event.as_ref(), last_count)
		{
			let presence = last_event
				.as_ref()
				.map(|event| &event.content.presence)
				.unwrap_or(state);

			self.schedule_presence_timer(user_id, presence, count)
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

		// 5) If we just transitioned away from online, flush suppressed pushes.
		if matches!(last_state, Some(PresenceState::Online))
			&& aggregated.state != PresenceState::Online
		{
			debug!(
				?user_id,
				from = ?PresenceState::Online,
				to = ?aggregated.state,
				"Presence went inactive; flushing suppressed pushes"
			);
			self.services
				.sending
				.schedule_flush_suppressed_for_user(
					user_id.to_owned(),
					"presence->inactive (aggregate)",
				);
		}

		// 6) Persist the aggregated presence, preserving last non-empty status.
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
		)
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
			Some(REFRESH_TIMEOUT),
		);

		debug!(?user_id, ?new_state, currently_active, "Presence ping accepted");

		futures::future::try_join(set_presence, update_device_seen.unwrap_or(Ok(())))
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
	) -> Result {
		let currently_active = *state == PresenceState::Online;
		self.apply_device_presence_update(
			user_id,
			Self::device_key(device_id, false),
			state,
			Some(currently_active),
			None,
			status_msg,
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
	) -> Result {
		self.apply_device_presence_update(
			user_id,
			Self::device_key(None, true),
			state,
			Some(currently_active),
			Some(last_active_ago),
			status_msg,
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
			let is_local = self.services.globals.user_is_local(user_id);
			let is_server_user = user_id == self.services.globals.server_user;
			let allow_timeout = self.timeout_remote_users || is_local;

			if allow_timeout && !is_server_user {
				self.schedule_presence_timer(user_id, presence_state, count)?;
			}
		}

		Ok(())
	}

	pub(super) async fn process_presence_timer(
		&self,
		user_id: &OwnedUserId,
		expected_count: u64,
	) -> Result {
		let Ok((current_count, presence)) = self.db.get_presence_raw(user_id).await else {
			return Ok(());
		};

		if Self::timer_is_stale(expected_count, current_count) {
			trace!(?user_id, expected_count, current_count, "Skipping stale presence timer");
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
				"Processed presence timer for user '{user_id}': Old state = {presence_state}, \
				 New state = {new_state:?}"
			);

			if let Some(new_state) = new_state {
				if matches!(new_state, PresenceState::Unavailable | PresenceState::Offline) {
					self.services
						.sending
						.schedule_flush_suppressed_for_user(
							user_id.to_owned(),
							"presence->inactive",
						);
				}
				self.set_presence(user_id, &new_state, Some(false), last_active_ago, status_msg)
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

		if matches!(aggregated.state, PresenceState::Unavailable | PresenceState::Offline) {
			self.services
				.sending
				.schedule_flush_suppressed_for_user(user_id.to_owned(), "presence->inactive");
		}

		let status_msg = aggregated
			.status_msg
			.or_else(|| presence.status_msg());
		let last_active_ago =
			Some(UInt::new_saturating(now.saturating_sub(aggregated.last_active_ts)));

		self.set_presence(
			user_id,
			&aggregated.state,
			Some(aggregated.currently_active),
			last_active_ago,
			status_msg,
		)
		.await?;

		Ok(())
	}
}

pub(super) async fn presence_timer(
	user_id: OwnedUserId,
	timeout: Duration,
	count: u64,
) -> TimerFired {
	sleep(timeout).await;

	(user_id, count)
}

#[cfg(test)]
mod tests {
	use ruma::{presence::PresenceState, uint, user_id};

	use super::*;

	#[test]
	fn refresh_window_skip_decision() {
		let user_id = user_id!("@alice:example.com");
		let event = PresenceEvent {
			sender: user_id.to_owned(),
			content: ruma::events::presence::PresenceEventContent {
				presence: PresenceState::Online,
				status_msg: None,
				currently_active: Some(true),
				last_active_ago: Some(uint!(10)),
				avatar_url: None,
				displayname: None,
			},
		};

		let decision = Service::refresh_skip_decision(Some(20), Some(&event), Some(5));
		assert_eq!(decision, Some((5, 10)));

		let decision = Service::refresh_skip_decision(Some(5), Some(&event), Some(5));
		assert_eq!(decision, None);

		let event_missing_ago = PresenceEvent {
			sender: user_id.to_owned(),
			content: ruma::events::presence::PresenceEventContent {
				presence: PresenceState::Online,
				status_msg: None,
				currently_active: Some(true),
				last_active_ago: None,
				avatar_url: None,
				displayname: None,
			},
		};

		let decision =
			Service::refresh_skip_decision(Some(20), Some(&event_missing_ago), Some(5));
		assert_eq!(decision, None);

		let decision = Service::refresh_skip_decision(Some(20), None, Some(5));
		assert_eq!(decision, None);
	}

	#[test]
	fn timer_stale_detection() {
		assert!(Service::timer_is_stale(2, 3));
		assert!(!Service::timer_is_stale(2, 2));
	}
}
