use ruma::{
	OwnedUserId, UInt, UserId,
	api::{
		client::push::{Pusher, PusherKind},
		push_gateway::send_event_notification::v1::Device,
	},
	push::HttpPusherData,
};
use serde::Serialize;
use tuwunel_core::{Result, implement, warn};
use tuwunel_database::Deserialized;

/// Schedule one non-blocking badge fanout after a successful unread reset.
#[implement(super::Service)]
pub fn schedule_badge_update(&self, user_id: OwnedUserId) {
	let pusher = self.services.pusher.clone();
	let runtime = self.services.server.runtime();
	runtime.spawn(async move {
		pusher.send_badge_notices(&user_id).await;
	});
}

#[implement(super::Service)]
async fn send_badge_notices(&self, user_id: &UserId) {
	for pusher in self.get_pushers(user_id).await {
		if let Err(e) = self.send_badge_notice(user_id, &pusher).await {
			warn!(%user_id, pushkey = %pusher.ids.pushkey, "Failed to send updated push badge: {e}");
		}
	}
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn send_badge_notice(&self, user_id: &UserId, pusher: &Pusher) -> Result {
	let pushkey = pusher.ids.pushkey.clone();
	let mutex_key = (user_id.to_owned(), pushkey.clone());
	let _lock = self.badge_send_mutex.lock(&mutex_key).await;
	let Ok(pusher) = self.get_pusher(user_id, &pushkey).await else {
		return Ok(());
	};
	let PusherKind::Http(http) = &pusher.kind else {
		return Ok(());
	};

	if badge_count_disabled(http) {
		return Ok(());
	}

	let unread = self.badge_count(user_id).await;
	let unread_state: u64 = unread.into();
	if self.last_badge(user_id, &pushkey).await == Some(unread_state) {
		return Ok(());
	}

	let mut device = Device::new(pusher.ids.app_id.clone(), pushkey.clone());
	device.data.data.clone_from(&http.data);
	device.data.format.clone_from(&http.format);

	let body = serde_json::to_vec(&BadgeRequest {
		notification: BadgeNotification {
			counts: BadgeCounts { unread },
			devices: vec![device],
		},
	})?;
	let response = self.send_raw_request(&http.url, body).await?;

	if response.rejected.contains(&pushkey) {
		warn!(url = %http.url, %pushkey, "Push gateway rejected the pushkey; removing pusher");
		self.delete_pusher_unlocked(user_id, &pushkey)
			.await;
	} else {
		self.store_last_badge(user_id, &pushkey, unread_state);
	}

	Ok(())
}

#[implement(super::Service)]
pub(super) async fn badge_count(&self, user_id: &UserId) -> UInt {
	self.global_notification_count(user_id)
		.await
		.try_into()
		.unwrap_or(UInt::MAX)
}

#[implement(super::Service)]
async fn last_badge(&self, user_id: &UserId, pushkey: &str) -> Option<u64> {
	self.db
		.senderkey_lastbadge
		.qry(&(user_id, pushkey))
		.await
		.deserialized()
		.ok()
}

#[implement(super::Service)]
pub(super) fn store_last_badge(&self, user_id: &UserId, pushkey: &str, unread: u64) {
	self.db
		.senderkey_lastbadge
		.put((user_id, pushkey), unread);
}

pub(super) fn badge_count_disabled(http: &HttpPusherData) -> bool {
	["org.matrix.msc4076.disable_badge_count", "disable_badge_count"]
		.into_iter()
		.any(|key| {
			http.data
				.get(key)
				.and_then(serde_json::Value::as_bool)
				== Some(true)
		})
}

/// Counts-only wire types deliberately do not skip zero. Ruma's general
/// `NotificationCounts` serializer does, but unread=0 is the badge-clear
/// signal.
#[derive(Serialize)]
struct BadgeRequest {
	notification: BadgeNotification,
}

#[derive(Serialize)]
struct BadgeNotification {
	counts: BadgeCounts,
	devices: Vec<Device>,
}

#[derive(Serialize)]
struct BadgeCounts {
	unread: UInt,
}

#[cfg(test)]
mod tests {
	use ruma::{api::push_gateway::send_event_notification::v1::Device, uint};
	use serde_json::json;

	use super::{BadgeCounts, BadgeNotification, BadgeRequest};

	#[test]
	fn zero_badge_payload_is_explicit_and_eventless() {
		let payload = BadgeRequest {
			notification: BadgeNotification {
				counts: BadgeCounts { unread: uint!(0) },
				devices: vec![Device::new("app".into(), "pushkey".into())],
			},
		};

		assert_eq!(
			serde_json::to_value(payload).expect("badge payload serializes"),
			json!({
				"notification": {
					"counts": { "unread": 0 },
					"devices": [{ "app_id": "app", "pushkey": "pushkey" }],
				}
			})
		);
	}
}
