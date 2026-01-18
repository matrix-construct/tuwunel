use futures::future::join;
use ruma::{
	UInt, UserId,
	events::presence::{PresenceEvent, PresenceEventContent},
	presence::PresenceState,
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Error, Result, utils, utils::future::TryExtExt};

use crate::users;

/// Represents data required to be kept in order to implement the presence
/// specification.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(super) struct Presence {
	state: PresenceState,
	currently_active: bool,
	last_active_ts: u64,
	status_msg: Option<String>,
}

impl Presence {
	#[must_use]
	pub(super) fn new(
		state: PresenceState,
		currently_active: bool,
		last_active_ts: u64,
		status_msg: Option<String>,
	) -> Self {
		Self {
			state,
			currently_active,
			last_active_ts,
			status_msg,
		}
	}

	pub(super) fn from_json_bytes(bytes: &[u8]) -> Result<Self> {
		serde_json::from_slice(bytes)
			.map_err(|_| Error::bad_database("Invalid presence data in database"))
	}

	#[inline]
	pub(super) fn state(&self) -> &PresenceState { &self.state }

	#[inline]
	pub(super) fn last_active_ts(&self) -> u64 { self.last_active_ts }

	#[inline]
	pub(super) fn status_msg(&self) -> Option<String> { self.status_msg.clone() }

	/// Creates a PresenceEvent from available data.
	pub(super) async fn to_presence_event(
		&self,
		user_id: &UserId,
		users: &users::Service,
	) -> PresenceEvent {
		let now = utils::millis_since_unix_epoch();
		let last_active_ago = now.saturating_sub(self.last_active_ts);

		let avatar_url = users.avatar_url(user_id).ok();
		let displayname = users.displayname(user_id).ok();
		let (avatar_url, displayname) = join(avatar_url, displayname).await;

		PresenceEvent {
			sender: user_id.to_owned(),
			content: PresenceEventContent {
				presence: self.state.clone(),
				status_msg: self.status_msg.clone(),
				currently_active: Some(self.currently_active),
				last_active_ago: Some(UInt::new_saturating(last_active_ago)),
				avatar_url,
				displayname,
			},
		}
	}
}
