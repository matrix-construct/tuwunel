use ruma::{UserId, events::room::member::MembershipState};
use tuwunel_core::{
	implement,
	matrix::{Event, Pdu},
	utils::{BoolExt, result::LogErr},
};

/// MSC4025: the pruned clone of `pdu` for recipient `user_id`, or `None` to
/// serve the original.
#[implement(super::Service)]
pub async fn erased_view(&self, user_id: &UserId, pdu: &Pdu) -> Option<Pdu> {
	self.erased_for(user_id, pdu)
		.await
		.then_async(|| self.pruned(pdu))
		.await
		.flatten()
}

/// MSC4025: whether `pdu` serves pruned to `user_id`: its sender is erased
/// and the recipient was not joined in the room state at the event.
#[implement(super::Service)]
pub async fn erased_for(&self, user_id: &UserId, pdu: &Pdu) -> bool {
	self.services.users.is_erased(pdu.sender()).await
		&& self.user_membership_at_pdu(user_id, pdu).await != MembershipState::Join
}

/// Prune per the room version's redaction rules. The pruned form carries no
/// `redacted_because`; no redaction event exists for a serve-time erasure.
#[implement(super::Service)]
async fn pruned(&self, pdu: &Pdu) -> Option<Pdu> {
	let rules = self
		.services
		.state
		.get_room_version_rules(pdu.room_id())
		.await
		.log_err()
		.ok()?;

	pdu.redacted(&rules.redaction).log_err().ok()
}
