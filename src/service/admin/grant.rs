use futures::FutureExt;
use ruma::{
	UserId,
	events::{
		StateEventType,
		room::{
			member::{MembershipState, RoomMemberEventContent},
			message::RoomMessageEventContent,
			power_levels::RoomPowerLevelsEventContent,
		},
	},
};
use tuwunel_core::{
	Err, Result, debug_info, debug_warn, error, implement, matrix::pdu::PduBuilder,
};

/// Invite the user to the tuwunel admin room.
///
/// This is equivalent to granting server admin privileges.
#[implement(super::Service)]
pub async fn make_user_admin(&self, user_id: &UserId) -> Result {
	let Ok(room_id) = self.get_admin_room().await else {
		debug_warn!(
			"make_user_admin was called without an admin room being available or created"
		);
		return Ok(());
	};

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	if self
		.services
		.state_cache
		.is_joined(user_id, &room_id)
		.await
	{
		return Err!(debug_warn!("User is already joined in the admin room"));
	}

	if self
		.services
		.state_cache
		.is_invited(user_id, &room_id)
		.await
	{
		return Err!(debug_warn!("User is already pending an invitation to the admin room"));
	}

	// Use the server user to grant the new admin's power level
	let server_user = self.services.globals.server_user.as_ref();

	// if this is our local user, just forcefully join them in the room. otherwise,
	// invite the remote user.
	if self.services.globals.user_is_local(user_id) {
		debug_info!("Inviting local user {user_id} to admin room {room_id}");
		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(
					String::from(user_id),
					&RoomMemberEventContent::new(MembershipState::Invite),
				),
				server_user,
				&room_id,
				&state_lock,
			)
			.await?;

		debug_info!("Force joining local user {user_id} to admin room {room_id}");
		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(
					String::from(user_id),
					&RoomMemberEventContent::new(MembershipState::Join),
				),
				user_id,
				&room_id,
				&state_lock,
			)
			.await?;
	} else {
		debug_info!("Inviting remote user {user_id} to admin room {room_id}");
		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(
					user_id.to_string(),
					&RoomMemberEventContent::new(MembershipState::Invite),
				),
				server_user,
				&room_id,
				&state_lock,
			)
			.await?;
	}

	// Set power levels
	let mut room_power_levels = self
		.services
		.state_accessor
		.room_state_get_content::<RoomPowerLevelsEventContent>(
			&room_id,
			&StateEventType::RoomPowerLevels,
			"",
		)
		.await
		.unwrap_or_default();

	room_power_levels
		.users
		.insert(server_user.into(), 69420.into());
	room_power_levels
		.users
		.insert(user_id.into(), 100.into());

	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &room_power_levels),
			server_user,
			&room_id,
			&state_lock,
		)
		.await?;

	// Set room tag
	let room_tag = self
		.services
		.server
		.config
		.admin_room_tag
		.as_str();

	if !room_tag.is_empty()
		&& let Err(e) = self
			.services
			.account_data
			.set_room_tag(user_id, &room_id, room_tag.into(), None)
			.await
	{
		error!(?room_id, ?user_id, ?room_tag, "Failed to set tag for admin grant: {e}");
	}

	if self.services.server.config.admin_room_notices {
		let welcome_message = String::from(
			"## Thank you for trying out tuwunel!\n\nTuwunel is a continuation of conduwuit which was technically a hard fork of Conduit.\n\nHelpful links:\n> GitHub Repo: https://github.com/matrix-construct/tuwunel\n> Documentation: https://matrix-construct.github.io/tuwunel\n> Report issues: https://github.com/matrix-construct/tuwunel/issues\n\nFor a list of available commands, send the following message in this room: `!admin --help`",
		);

		// Send welcome message
		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder::timeline(&RoomMessageEventContent::text_markdown(welcome_message)),
				server_user,
				&room_id,
				&state_lock,
			)
			.await?;
	}

	Ok(())
}

/// Demote an admin, removing its rights.
#[implement(super::Service)]
pub async fn revoke_admin(&self, user_id: &UserId) -> Result {
	use MembershipState::{Invite, Join, Knock, Leave};

	let Ok(room_id) = self.get_admin_room().await else {
		return Err!(error!("No admin room available or created."));
	};

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	let event = match self
		.services
		.state_accessor
		.get_member(&room_id, user_id)
		.await
	{
		| Err(e) if e.is_not_found() => return Err!("{user_id} was never an admin."),

		| Err(e) => return Err!(error!(?e, "Failure occurred while attempting revoke.")),

		| Ok(event) if !matches!(event.membership, Invite | Knock | Join) => {
			return Err!("Cannot revoke {user_id} in membership state {:?}.", event.membership);
		},

		| Ok(event) => {
			assert!(
				matches!(event.membership, Invite | Knock | Join),
				"Incorrect membership state to remove user."
			);

			event
		},
	};

	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(user_id.to_string(), &RoomMemberEventContent {
				membership: Leave,
				reason: Some("Admin Revoked".into()),
				is_direct: None,
				join_authorized_via_users_server: None,
				third_party_invite: None,
				..event
			}),
			self.services.globals.server_user.as_ref(),
			&room_id,
			&state_lock,
		)
		.boxed()
		.await
		.map(|_| ())
}
