use std::{
	collections::BTreeMap,
	convert::identity,
	sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use futures::StreamExt;
use ruma::{
	OwnedUserId, ServerName, UserId,
	api::federation::transactions::edu::{Edu, PresenceContent, PresenceUpdate},
	events::presence::PresenceEventContent,
	uint,
};
use tuwunel_core::{
	implement,
	utils::{ReadyExt, result::LogErr, stream::TryReadyExt},
};

use super::edu_buf;
use crate::sending::{EDU_LIMIT, EduBuf, Service};

/// The latest presence state per user gathered for one EDU.
type Updates = BTreeMap<OwnedUserId, PresenceUpdate>;

const USER_LIMIT: usize = 256;

/// Select one presence EDU for the users a server may see.
///
/// The window's local presence transitions collapse into one `Edu::Presence`
/// carrying the latest state per user. A budget trip drops it, and presence
/// self-heals on the next transition.
#[implement(Service)]
#[tracing::instrument(
	name = "presence",
	level = "trace",
	skip(self, server_name, max_edu_count, events_len)
)]
pub(super) async fn select_edus_presence(
	&self,
	server_name: &ServerName,
	since: (u64, u64),
	max_edu_count: &AtomicU64,
	events_len: &AtomicUsize,
) -> Option<EduBuf> {
	// Sequential mapping parks the cursor while the item's borrows cross the awaits.
	let updates = self
		.services
		.presence
		.presence_since(since.0, Some(since.1))
		.inspect(|(_, count, _)| {
			debug_assert!(*count <= since.1, "exceeded upper-bound");
			max_edu_count.fetch_max(*count, Ordering::Relaxed);
		})
		.ready_filter(|(user_id, ..)| self.services.globals.user_is_local(user_id))
		.filter_map(|(user_id, _, presence_bytes)| {
			self.presence_update(server_name, user_id, presence_bytes)
		})
		.map(Ok)
		.ready_try_fold(Updates::new(), |mut updates, (user_id, update)| {
			updates.insert(user_id, update);

			// The distinct-user limit ends the scan with the map as the error.
			if updates.len() >= USER_LIMIT {
				Err(updates)
			} else {
				Ok(updates)
			}
		})
		.await
		.unwrap_or_else(identity);

	if updates.is_empty() {
		return None;
	}

	// A budget trip drops presence, which self-heals on the next transition.
	if events_len.fetch_add(1, Ordering::Relaxed) >= EDU_LIMIT {
		return None;
	}

	Some(edu_buf(&Edu::Presence(PresenceContent {
		push: updates.into_values().collect(),
	})))
}

/// The presence update to ship for one transition, if the server may see it.
#[implement(Service)]
async fn presence_update(
	&self,
	server_name: &ServerName,
	user_id: &UserId,
	presence_bytes: &[u8],
) -> Option<(OwnedUserId, PresenceUpdate)> {
	if !self
		.services
		.state_cache
		.server_sees_user(server_name, user_id)
		.await
	{
		return None;
	}

	let PresenceEventContent {
		presence,
		currently_active,
		status_msg,
		last_active_ago,
		..
	} = self
		.services
		.presence
		.from_json_bytes_to_event(presence_bytes, user_id)
		.await
		.log_err()
		.ok()?
		.content;

	let update = PresenceUpdate {
		user_id: user_id.to_owned(),
		presence,
		currently_active: currently_active.unwrap_or(false),
		status_msg,
		last_active_ago: last_active_ago.unwrap_or_else(|| uint!(0)),
	};

	Some((user_id.to_owned(), update))
}
