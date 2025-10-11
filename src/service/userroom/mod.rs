use std::sync::{Arc, OnceLock};

use ruma::{
	EventId, OwnedRoomAliasId, OwnedRoomId, RoomAliasId, RoomId, UserId,
	events::room::{
		guest_access::GuestAccess,
		member::{MembershipState, RoomMemberEventContent},
		message::RoomMessageEventContent,
	},
	room::JoinRule,
};
use tuwunel_core::{Result, debug_info, debug_warn, pdu::PduBuilder};

use crate::command::{CommandResult, CommandSystem};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	user_command_system: OnceLock<Arc<dyn CommandSystem>>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			user_command_system: OnceLock::new(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	pub fn get_user_room_alias(&self, user_id: &UserId) -> OwnedRoomAliasId {
		self.services
			.globals
			.local_alias(&format!("{}-userroom", user_id.localpart()))
			.unwrap()
	}

	pub async fn get_user_room(&self, user_id: &UserId) -> Result<OwnedRoomId> {
		let user_room_alias = self.get_user_room_alias(user_id);
		self.services
			.alias
			.resolve_local_alias(&user_room_alias)
			.await
	}

	pub fn is_user_room_alias(&self, alias: &RoomAliasId) -> bool {
		self.services.globals.alias_is_local(alias) && alias.alias().ends_with("-userroom")
	}

	pub async fn create_user_room(&self, user_id: &UserId) -> Result {
		let server_user = &self.services.globals.server_user;
		let alias = self.get_user_room_alias(user_id);
		let name = format!("User Room of {user_id}");
		let topic = format!("eeeeee .-.");
		let (room_id, state_lock) = self
			.services
			.create
			.create_room(
				&server_user,
				None,
				None,
				Some(&alias),
				&[],
				false,
				Vec::new(),
				JoinRule::Invite,
				GuestAccess::Forbidden,
				false,
				Some(&name),
				Some(&topic),
				None,
				None,
			)
			.await?;

		debug_info!("Inviting user {user_id} to user room {room_id}");
		self.services
			.timeline
			.build_and_append_pdu_without_retention(
				PduBuilder::state(
					String::from(user_id),
					&RoomMemberEventContent::new(MembershipState::Invite),
				),
				server_user,
				&room_id,
				&state_lock,
			)
			.await?;

		debug_info!("Force joining user {user_id} to user room {room_id}");
		self.services
			.timeline
			.build_and_append_pdu_without_retention(
				PduBuilder::state(
					String::from(user_id),
					&RoomMemberEventContent::new(MembershipState::Join),
				),
				user_id,
				&room_id,
				&state_lock,
			)
			.await?;

		Ok(())
	}

	pub async fn send_text(&self, user_id: &UserId, body: &str) -> Result {
		if !self.services.globals.user_is_local(user_id) {
			debug_info!(%user_id, "Skipping user room send for remote user");
			return Ok(());
		}

		let room_id = match self.get_user_room(user_id).await {
			| Ok(room_id) => room_id,
			| Err(e) => {
				debug_warn!(%user_id, error = %e, "User room missing; unable to deliver message");
				return Ok(());
			},
		};

		let state_lock = self.services.state.mutex.lock(&room_id).await;
		let content = RoomMessageEventContent::text_markdown(body);

		self.services
			.timeline
			.build_and_append_pdu_without_retention(
				PduBuilder::timeline(&content),
				&self.services.globals.server_user,
				&room_id,
				&state_lock,
			)
			//.boxed()
			.await?;

		Ok(())
	}

	pub async fn message_hook(
		&self,
		event_id: &EventId,
		room_id: &RoomId,
		sender: &UserId,
		command: &str,
	) {
		if !self.services.globals.user_is_local(sender) {
			return;
		}

		if !self
			.get_user_room(sender)
			.await
			.is_ok_and(|user_room| room_id == user_room)
		{
			return;
		}

		if !command.starts_with("!user") {
			return;
		}

		let command = &command[1..];

		self.services.command.run_command_matrix_detached(
			self.get_user_command_system(),
			event_id,
			room_id,
			command,
			sender,
			sender,
			None,
		);
	}

	pub async fn run_command(
		&self,
		command: &str,
		input: &str,
		user_id: &UserId,
	) -> CommandResult {
		self.services
			.command
			.run_command(self.get_user_command_system().as_ref(), command, input, Some(user_id))
			.await
	}

	pub fn set_user_command_system(&self, command_system: Arc<dyn CommandSystem>) {
		self.user_command_system
			.set(command_system)
			.ok()
			.expect("user command system already initialized");
	}

	fn get_user_command_system(&self) -> &Arc<dyn CommandSystem> {
		self.user_command_system
			.get()
			.expect("user command system empty")
	}
}
