//! Acceptance of incoming invites on behalf of local users.
//!
//! Arrival sites queue an invite from inside the room's state lock; the
//! service worker performs the joins once that lock is gone.

use std::time::Duration;

use futures::StreamExt;
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use tokio::time::sleep;
use tuwunel_core::{
	debug, debug_warn, implement,
	utils::{
		future::{ReadyBoolExt, and5},
		result::LogErr,
		stream::automatic_width,
	},
};

use super::{Join, Service};

/// One invite waiting for the worker to act on it.
///
/// The identifiers are owned rather than borrowed because the queue outlives
/// the arrival site that filled it, which holds them only for the length of
/// its own critical section.
pub(super) struct Pending {
	room_id: OwnedRoomId,
	user_id: OwnedUserId,
	sender: OwnedUserId,
	is_direct: bool,
}

/// Join attempts made before the invite is left for its user to answer.
///
/// The first attempt runs immediately and each retry doubles the wait from
/// one second. An invite is answered over federation before the inviting
/// server has applied it to its own state, so an early join can arrive there
/// unauthorized.
const ATTEMPTS: u32 = 5;

/// Queue an arriving invite for acceptance on the invited user's behalf.
///
/// Configured policy and the two users' locality decide whether it is queued
/// at all. Nothing is awaited and nothing is locked, so an arrival site can
/// call this while it holds the room's state mutex.
#[implement(Service)]
pub fn auto_accept(&self, room_id: &RoomId, user_id: &UserId, sender: &UserId, is_direct: bool) {
	let config = &self.services.config;

	let accepts = config.auto_accept_invites
		&& (is_direct || !config.auto_accept_invites_direct_only)
		&& self.services.globals.user_is_local(user_id)
		&& (!config.auto_accept_invites_local_only
			|| self.services.globals.user_is_local(sender));

	if !accepts {
		return;
	}

	self.queue
		.0
		.send(Pending {
			room_id: room_id.to_owned(),
			user_id: user_id.to_owned(),
			sender: sender.to_owned(),
			is_direct,
		})
		.ok();
}

/// Service worker, joining queued invites up to the concurrency band.
///
/// The queue itself stays unbounded so an arrival site never blocks under the
/// room's state mutex. Shutdown abandons whatever is in flight, which leaves
/// the invite for its user to answer.
#[implement(Service)]
pub(super) async fn accept_worker(&self) {
	let accepting = self
		.queue
		.1
		.stream()
		.for_each_concurrent(automatic_width(), async |invite| self.accept(invite).await);

	tokio::select! {
		() = accepting => {},
		() = self.services.server.until_shutdown() => {},
	}
}

#[implement(Service)]
#[tracing::instrument(
	name = "auto_accept",
	level = "debug",
	skip_all,
	fields(
		%room_id,
		%user_id,
	),
)]
async fn accept(&self, Pending { room_id, user_id, sender, is_direct }: Pending) {
	if !self.join_invited(&room_id, &user_id).await {
		return;
	}

	debug!("Accepted the invitation on the user's behalf.");

	if is_direct {
		self.services
			.account_data
			.mark_direct(&user_id, &sender, &room_id)
			.await
			.log_err()
			.ok();
	}
}

/// Join the room, reattempting for as long as the invite still stands.
///
/// Withdrawing the invite ends the attempts, and so does the last delay
/// elapsing, which leaves the room for its user to join by hand.
#[implement(Service)]
async fn join_invited(&self, room_id: &RoomId, user_id: &UserId) -> bool {
	for attempt in 0..ATTEMPTS {
		let delay = attempt
			.checked_sub(1)
			.map_or(Duration::ZERO, |retry| Duration::from_secs(1 << retry));

		sleep(delay).await;

		if !self.acceptable(room_id, user_id).await {
			return false;
		}

		let joined = self
			.join(Join {
				sender_user: user_id,
				room_id,
				orig_room_id: None,
				reason: None,
				servers: &[],
				is_appservice: false,
				extra_content: None,
			})
			.await;

		match joined {
			| Ok(()) => return true,
			| Err(e) => debug_warn!(?e, "Automatic invite acceptance attempt failed"),
		}
	}

	false
}

/// Whether the invite still stands and its user is one we may act for.
///
/// Deactivated, suspended, and locked accounts are refused their own requests
/// by the middleware, so answering an invite for one would route around it.
/// A user whose `m.invite_permission_config` blocks invites never sees the
/// stored invite in sync (MSC4380), and a join on their behalf would surface
/// the room regardless, so blocked users are skipped as well. Every attempt
/// re-reads this, since the delays leave room for any of it to change
/// underneath.
///
/// The conjunction short-circuits, so the invite leads as the loop's own
/// exit condition, and `invites_blocked` trails on two chained reads and a
/// deserialization.
#[implement(Service)]
async fn acceptable(&self, room_id: &RoomId, user_id: &UserId) -> bool {
	let state_cache = &self.services.state_cache;
	let users = &self.services.users;

	let invited = state_cache.is_invited(user_id, room_id);
	let active = users.is_active_local(user_id);
	let unsuspended = users.is_suspended(user_id).is_false();
	let unlocked = users.is_locked(user_id).is_false();
	let unblocked = users.invites_blocked(user_id).is_false();

	and5(invited, active, unsuspended, unlocked, unblocked).await
}
