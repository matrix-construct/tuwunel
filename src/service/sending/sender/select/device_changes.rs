use std::{
	collections::BTreeSet,
	sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use futures::{StreamExt, pin_mut};
use ruma::{
	ServerName, UserId,
	api::federation::transactions::edu::{DeviceListUpdateContent, Edu},
	device_id, uint,
};
use tuwunel_core::{implement, utils::ReadyExt};

use super::{Selected, edu_buf};
use crate::sending::{EduBuf, Service};

/// Select one device-list EDU per local user whose keys changed in the window.
///
/// The EDU carries placeholder content: an empty `prev_id` makes the receiver
/// resync the device list, so only the user ID has to be right.
#[implement(Service)]
#[tracing::instrument(
	name = "device_changes",
	level = "trace",
	skip(self, server_name, max_edu_count, events_len)
)]
pub(super) async fn select_edus_device_changes(
	&self,
	server_name: &ServerName,
	since: (u64, u64),
	max_edu_count: &AtomicU64,
	events_len: &AtomicUsize,
) -> Selected {
	let server_rooms = self
		.services
		.state_cache
		.server_rooms(server_name);

	pin_mut!(server_rooms);
	let mut acc = (Selected::default(), BTreeSet::new()); // fold state threaded through the rooms
	while let Some(room_id) = server_rooms.next().await {
		acc = self
			.services
			.users
			.room_keys_changed(room_id, since.0, Some(since.1))
			.ready_fold(acc, |(mut selected, mut seen), (user_id, count)| {
				if !self.services.globals.user_is_local(user_id) {
					return (selected, seen);
				}

				debug_assert!(count <= since.1, "exceeds upper-bound");
				max_edu_count.fetch_max(count, Ordering::Relaxed);

				// Overflow replay is benign: the placeholder content is user-id-only.
				if seen.insert(user_id.to_owned()) {
					selected.push(device_list_edu(user_id), events_len);
				}

				(selected, seen)
			})
			.await;
	}

	let (selected, _) = acc;

	selected
}

fn device_list_edu(user_id: &UserId) -> EduBuf {
	edu_buf(&Edu::DeviceListUpdate(DeviceListUpdateContent {
		user_id: user_id.to_owned(),
		device_id: device_id!("placeholder").to_owned(),
		device_display_name: Some("Placeholder".to_owned()),
		stream_id: uint!(1),
		prev_id: Vec::new(),
		deleted: None,
		keys: None,
	}))
}
