use std::collections::BTreeMap;

use futures::FutureExt;
use ruma::{
	RoomId, RoomVersionId,
	events::room::{
		canonical_alias::RoomCanonicalAliasEventContent,
		create::RoomCreateEventContent,
		guest_access::{GuestAccess, RoomGuestAccessEventContent},
		history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
		join_rules::{JoinRule, RoomJoinRulesEventContent},
		member::{MembershipState, RoomMemberEventContent},
		name::RoomNameEventContent,
		power_levels::RoomPowerLevelsEventContent,
		preview_url::RoomPreviewUrlsEventContent,
		topic::{RoomTopicEventContent, TopicContentBlock},
	},
};
use tuwunel_core::{Result, pdu::PduBuilder};

use crate::Services;

/// Create the server user.
///
/// This should be the first user on the server and created prior to the
/// admin room.
pub async fn create_server_user(services: &Services) -> Result {
	let server_user = services.globals.server_user.as_ref();

	// Create a user for the server
	services
		.users
		.create(server_user, None, None)
		.await?;

	Ok(())
}

/// Create the admin room.
///
/// Users in this room are considered admins by tuwunel, and the room can be
/// used to issue admin commands by talking to the server user inside it.
pub async fn create_admin_room(services: &Services) -> Result {
	let room_id = RoomId::new_v1(services.globals.server_name());
	let room_version = RoomVersionId::V11;

	let _short_id = services
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	let state_lock = services.state.mutex.lock(&room_id).await;

	// Create a user for the server
	let server_user = services.globals.server_user.as_ref();
	if !services.users.exists(server_user).await {
		create_server_user(services).await?;
	}

	let create_content = {
		use RoomVersionId::*;
		match room_version {
			| V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 =>
				RoomCreateEventContent::new_v1(server_user.into()),
			| _ => RoomCreateEventContent::new_v11(),
		}
	};

	// 1. The room create event
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomCreateEventContent {
				federate: services.config.federate_admin_room,
				predecessor: None,
				room_version: room_version.clone(),
				..create_content
			}),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 2. Make server user/bot join
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::from(server_user),
				&RoomMemberEventContent::new(MembershipState::Join),
			),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 3. Power levels
	let users = BTreeMap::from_iter([(server_user.into(), 69420.into())]);

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomPowerLevelsEventContent {
				users,
				..Default::default()
			}),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 4.1 Join Rules
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomJoinRulesEventContent::new(JoinRule::Invite)),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 4.2 History Visibility
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::new(),
				&RoomHistoryVisibilityEventContent::new(HistoryVisibility::Shared),
			),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 4.3 Guest Access
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::new(),
				&RoomGuestAccessEventContent::new(GuestAccess::Forbidden),
			),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 5. Events implied by name and topic
	let room_name = format!("{} Admin Room", services.config.server_name);
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomNameEventContent::new(room_name)),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomTopicEventContent {
				topic_block: TopicContentBlock::default(),
				topic: format!("Manage {} | Run commands prefixed with `!admin` | Run `!admin -h` for help | Documentation: https://matrix-construct.github.io/tuwunel", services.config.server_name),
			}),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 6. Room alias
	let alias = &services.admin.admin_alias;

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomCanonicalAliasEventContent {
				alias: Some(alias.clone()),
				alt_aliases: Vec::new(),
			}),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	services.alias.set_alias(alias, &room_id)?;

	// 7. (ad-hoc) Disable room URL previews for everyone by default
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomPreviewUrlsEventContent { disabled: true }),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	Ok(())
}
