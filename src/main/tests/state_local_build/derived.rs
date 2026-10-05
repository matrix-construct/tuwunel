use tuwunel_core::{
	Result, async_noinline,
	ruma::{
		RoomId, UserId,
		events::{StateEventType, room::member::MembershipState},
	},
};
use tuwunel_service::Services;

use super::helpers::{
	append_membership, append_message, append_state, assert_accepts_derived, restore_room_state,
	set_forward_extremities, sign_message,
};
use crate::client::register;

// size firewall
#[async_noinline]
pub(super) async fn sibling_state_prevs_both_resolve<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let context = "sibling state prevs";
	let joiner =
		register(services, "siblingjoiner", "state-local-build-access-token-0002").await?;

	append_membership(services, user_id, room_id, &joiner, MembershipState::Invite).await?;

	let parent = append_message(services, user_id, room_id, "sibling parent").await?;
	let parent_before = services.state.pdu_shortstatehash(&parent).await?;
	let join =
		append_membership(services, &joiner, room_id, &joiner, MembershipState::Join).await?;

	let join_after = services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	restore_room_state(services, room_id, parent_before, &parent).await;

	let rename = append_state(services, user_id, room_id, "sibling rename").await?;
	let join_before = services.state.pdu_shortstatehash(&join).await?;
	let rename_before = services.state.pdu_shortstatehash(&rename).await?;

	// Both siblings set state over one snapshot, so dropping either fork fails below.
	assert_eq!(join_before, rename_before, "{context} fixture states diverged");

	restore_room_state(services, room_id, join_after, &join).await;
	set_forward_extremities(services, room_id, [join.as_ref(), rename.as_ref()]).await;

	let (top, top_json) = sign_message(services, &joiner, room_id, "sibling top").await?;

	assert_eq!(top.prev_events.len(), 2, "{context} fixture did not fork");
	assert_accepts_derived(services, room_id, &top, top_json, context).await?;

	let top_before = services
		.state
		.pdu_shortstatehash(&top.event_id)
		.await?;

	let member = services
		.state_accessor
		.state_get_id(top_before, &StateEventType::RoomMember, joiner.as_str())
		.await?;

	let name = services
		.state_accessor
		.state_get_id(top_before, &StateEventType::RoomName, "")
		.await?;

	assert_eq!(member, join, "{context} lost the join");
	assert_eq!(name, rename, "{context} lost the rename");

	Ok(())
}
