//! Membership updates for the tests that seed the state cache directly.
//!
//! Only a binary declaring it by path compiles it, so the boot fixture's other
//! binaries stay free of its dead code.

use tuwunel_core::ruma::{
	RoomId, UserId,
	events::room::member::{MembershipState, RoomMemberEventContent},
};
use tuwunel_matrix::PduCount;
use tuwunel_service::rooms::state_cache::MembershipUpdate;

/// Builds a state-cache membership update with no event behind it.
///
/// It is the user's own transition to a bare membership, with the defaults of
/// `MembershipUpdate::self_sent`. A caller taking the count from `next_count`
/// must hold its permit until the update is awaited, so the row lands before
/// the count retires.
pub(crate) fn membership_update<'a>(
	room_id: &'a RoomId,
	user_id: &'a UserId,
	membership: MembershipState,
	count: u64,
) -> MembershipUpdate<'a> {
	let content = RoomMemberEventContent::new(membership);

	MembershipUpdate::self_sent(room_id, user_id, content, PduCount::Normal(count))
}
