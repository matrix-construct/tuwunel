//! Presence aggregation across devices.
//!
//! This module keeps per-device presence snapshots and computes a single
//! user-level presence view. Aggregation applies idle/offline thresholds,
//! favors higher-ranked states, and prunes stale devices to cap memory.

use std::collections::HashMap;

use ruma::{OwnedDeviceId, OwnedUserId, UInt, UserId, presence::PresenceState};
use tokio::sync::RwLock;
use tuwunel_core::debug;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum DeviceKey {
	Device(OwnedDeviceId),
	Remote,
	UnknownLocal,
}

#[derive(Debug, Clone)]
struct DevicePresence {
	state: PresenceState,
	currently_active: bool,
	last_active_ts: u64,
	last_update_ts: u64,
	status_msg: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AggregatedPresence {
	pub(crate) state: PresenceState,
	pub(crate) currently_active: bool,
	pub(crate) last_active_ts: u64,
	pub(crate) status_msg: Option<String>,
	pub(crate) device_count: usize,
}

#[derive(Debug, Default)]
pub(crate) struct PresenceAggregator {
	inner: RwLock<HashMap<OwnedUserId, HashMap<DeviceKey, DevicePresence>>>,
}

impl PresenceAggregator {
	/// Create a new, empty aggregator.
	pub(crate) fn new() -> Self { Self::default() }

	/// Clear all tracked device state.
	pub(crate) async fn clear(&self) { self.inner.write().await.clear(); }

	/// Update presence state for a single device.
	#[expect(clippy::too_many_arguments)]
	pub(crate) async fn update(
		&self,
		user_id: &UserId,
		device_key: DeviceKey,
		state: &PresenceState,
		currently_active: Option<bool>,
		last_active_ago: Option<UInt>,
		status_msg: Option<String>,
		now_ms: u64,
	) {
		let mut guard = self.inner.write().await;
		let devices = guard.entry(user_id.to_owned()).or_default();

		let last_active_ts = match last_active_ago {
			| None => now_ms,
			| Some(ago) => now_ms.saturating_sub(ago.into()),
		};

		let entry = devices
			.entry(device_key)
			.or_insert_with(|| DevicePresence {
				state: state.clone(),
				currently_active: currently_active.unwrap_or(false),
				last_active_ts,
				last_update_ts: now_ms,
				status_msg: status_msg.clone(),
			});

		entry.state = state.clone();
		entry.currently_active = currently_active.unwrap_or(false);
		entry.last_active_ts = last_active_ts;
		entry.last_update_ts = now_ms;
		if status_msg.is_some() {
			entry.status_msg = status_msg;
		}
	}

	/// Aggregate per-device state into a single presence snapshot.
	///
	/// Prunes devices that have not updated within the offline timeout to keep
	/// the map bounded.
	pub(crate) async fn aggregate(
		&self,
		user_id: &UserId,
		now_ms: u64,
		idle_timeout_ms: u64,
		offline_timeout_ms: u64,
	) -> AggregatedPresence {
		let mut guard = self.inner.write().await;
		let Some(devices) = guard.get_mut(user_id) else {
			return AggregatedPresence {
				state: PresenceState::Offline,
				currently_active: false,
				last_active_ts: now_ms,
				status_msg: None,
				device_count: 0,
			};
		};

		let mut best_state = PresenceState::Offline;
		let mut best_rank = state_rank(&best_state);
		let mut any_currently_active = false;
		let mut last_active_ts = 0_u64;
		let mut latest_status: Option<(u64, String)> = None;

		devices.retain(|_, device| {
			let last_active_age = now_ms.saturating_sub(device.last_active_ts);
			let last_update_age = now_ms.saturating_sub(device.last_update_ts);

			let effective_state = effective_device_state(
				&device.state,
				last_active_age,
				idle_timeout_ms,
				offline_timeout_ms,
			);

			let rank = state_rank(&effective_state);
			if rank > best_rank {
				best_rank = rank;
				best_state = effective_state.clone();
			}

			if (effective_state == PresenceState::Online
				|| effective_state == PresenceState::Busy)
				&& device.currently_active
				&& last_active_age < idle_timeout_ms
			{
				any_currently_active = true;
			}

			if let Some(msg) = device
				.status_msg
				.as_ref()
				.filter(|msg| !msg.is_empty())
			{
				match latest_status {
					| None => {
						latest_status = Some((device.last_update_ts, msg.clone()));
					},
					| Some((ts, _)) if device.last_update_ts > ts => {
						latest_status = Some((device.last_update_ts, msg.clone()));
					},
					| _ => {},
				}
			}

			if device.last_active_ts > last_active_ts {
				last_active_ts = device.last_active_ts;
			}

			// Drop devices that haven't updated in a long time to keep the map small.
			last_update_age < offline_timeout_ms
		});

		let device_count = devices.len();
		let status_msg = latest_status.map(|(_, msg)| msg);

		if device_count == 0 {
			guard.remove(user_id);
			return AggregatedPresence {
				state: PresenceState::Offline,
				currently_active: false,
				last_active_ts: now_ms,
				status_msg: None,
				device_count: 0,
			};
		}

		debug!(
			?user_id,
			device_count,
			state = ?best_state,
			currently_active = any_currently_active,
			last_active_ts,
			status_msg = status_msg.as_deref(),
			"Aggregated presence"
		);

		AggregatedPresence {
			state: best_state,
			currently_active: any_currently_active,
			last_active_ts: if last_active_ts == 0 { now_ms } else { last_active_ts },
			status_msg,
			device_count,
		}
	}
}

fn effective_device_state(
	state: &PresenceState,
	last_active_age: u64,
	idle_timeout_ms: u64,
	offline_timeout_ms: u64,
) -> PresenceState {
	match state {
		| PresenceState::Busy | PresenceState::Online =>
			if last_active_age >= idle_timeout_ms {
				PresenceState::Unavailable
			} else {
				state.clone()
			},
		| PresenceState::Unavailable =>
			if last_active_age >= offline_timeout_ms {
				PresenceState::Offline
			} else {
				PresenceState::Unavailable
			},
		| PresenceState::Offline => PresenceState::Offline,
		| _ => state.clone(),
	}
}

fn state_rank(state: &PresenceState) -> u8 {
	match state {
		| PresenceState::Busy => 3,
		| PresenceState::Online => 2,
		| PresenceState::Unavailable => 1,
		| _ => 0,
	}
}

#[cfg(test)]
mod tests {
	use ruma::{device_id, uint, user_id};

	use super::*;

	#[tokio::test]
	async fn aggregates_rank_and_status_msg() {
		let aggregator = PresenceAggregator::new();
		let user = user_id!("@alice:example.com");
		let now = 1_000_u64;

		aggregator
			.update(
				user,
				DeviceKey::Device(device_id!("DEVICE_A").to_owned()),
				&PresenceState::Unavailable,
				Some(false),
				Some(uint!(50)),
				Some("away".to_owned()),
				now,
			)
			.await;

		aggregator
			.update(
				user,
				DeviceKey::Device(device_id!("DEVICE_B").to_owned()),
				&PresenceState::Online,
				Some(true),
				Some(uint!(10)),
				Some("online".to_owned()),
				now + 10,
			)
			.await;

		let aggregated = aggregator
			.aggregate(user, now + 10, 100, 300)
			.await;

		assert_eq!(aggregated.state, PresenceState::Online);
		assert!(aggregated.currently_active);
		assert_eq!(aggregated.status_msg.as_deref(), Some("online"));
		assert_eq!(aggregated.device_count, 2);
	}

	#[tokio::test]
	async fn degrades_online_to_unavailable_after_idle() {
		let aggregator = PresenceAggregator::new();
		let user = user_id!("@bob:example.com");
		let now = 10_000_u64;

		aggregator
			.update(
				user,
				DeviceKey::Device(device_id!("DEVICE_IDLE").to_owned()),
				&PresenceState::Online,
				Some(true),
				Some(uint!(500)),
				None,
				now,
			)
			.await;

		let aggregated = aggregator
			.aggregate(user, now + 500, 100, 1_000)
			.await;

		assert_eq!(aggregated.state, PresenceState::Unavailable);
	}

	#[tokio::test]
	async fn drops_stale_devices_on_aggregate() {
		let aggregator = PresenceAggregator::new();
		let user = user_id!("@carol:example.com");

		aggregator
			.update(
				user,
				DeviceKey::Device(device_id!("DEVICE_STALE").to_owned()),
				&PresenceState::Online,
				Some(true),
				Some(uint!(10)),
				None,
				0,
			)
			.await;

		let aggregated = aggregator.aggregate(user, 1_000, 100, 100).await;

		assert_eq!(aggregated.device_count, 0);
		assert_eq!(aggregated.state, PresenceState::Offline);
	}
}
