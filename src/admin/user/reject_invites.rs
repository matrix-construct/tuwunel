use futures::StreamExt;
use ruma::OwnedRoomId;
use tuwunel_core::{
	Err, Result, debug_warn,
	utils::{ReadyExt, stream::BroadbandExt},
};

use crate::{admin_command, utils::parse_local_user_id};

#[derive(Default)]
struct RejectInvitesAcc {
	rejected: usize,
	failed: usize,
}

impl RejectInvitesAcc {
	fn merge(mut self, Self { rejected, failed }: Self) -> Self {
		self.rejected = self.rejected.saturating_add(rejected);
		self.failed = self.failed.saturating_add(failed);
		self
	}
}

#[admin_command]
pub(super) async fn reject_invites(&self, user_id: String, reason: Option<String>) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;
	let reason = reason.as_deref();

	let reject = async |room_id: OwnedRoomId| {
		let state_lock = self.services.state.mutex.lock(&room_id).await;

		match self
			.services
			.membership
			.leave(&user_id, &room_id, reason.map(str::to_owned), false, &state_lock)
			.await
		{
			| Ok(()) => RejectInvitesAcc { rejected: 1, ..Default::default() },
			| Err(e) => {
				debug_warn!(%user_id, %room_id, "Failed to reject invite: {e}");
				RejectInvitesAcc { failed: 1, ..Default::default() }
			},
		}
	};

	let RejectInvitesAcc { rejected, failed } = self
		.services
		.state_cache
		.rooms_invited(&user_id)
		.map(ToOwned::to_owned)
		.broad_then(reject)
		.ready_fold(RejectInvitesAcc::default(), RejectInvitesAcc::merge)
		.await;

	if rejected == 0 && failed == 0 {
		return Err!("{user_id} has no pending invites.");
	}

	write!(self, "Rejected {rejected} invite(s) for {user_id}. {failed} failed.").await
}
