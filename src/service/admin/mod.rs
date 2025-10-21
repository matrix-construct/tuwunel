pub mod create;
mod execute;
mod grant;

use std::{
	str::FromStr,
	sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use futures::FutureExt;
use ruma::{
	EventId, OwnedRoomAliasId, OwnedRoomId, RoomId, UserId,
	events::room::message::RoomMessageEventContent,
};
use tracing::Level;
use tuwunel_core::{Result, err, pdu::PduBuilder, warn};

use crate::command::{CommandResult, CommandSystem};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	pub admin_alias: OwnedRoomAliasId,
	admin_command_system: OnceLock<Arc<dyn CommandSystem>>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			admin_alias: OwnedRoomAliasId::parse(format!("#admins:{}", &args.server.name))
				.expect("#admins:server_name is valid alias name"),
			admin_command_system: OnceLock::new(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		self.startup_execute().await?;

		Ok(())
	}

	async fn interrupt(&self) {}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[allow(clippy::enum_variant_names)]
enum AdminCommandCheckVerdict {
    NotAdminCommand,
    AdminEscapeCommand,
    AdminRoomCommand,
}


impl Service {
	/// Sends markdown notice to the admin room as the admin user.
	pub async fn notice(&self, body: &str) {
		self.send_message(RoomMessageEventContent::notice_markdown(body))
			.await
			.ok();
	}

	/// Sends markdown message (not an m.notice for notification reasons) to the
	/// admin room as the admin user.
	pub async fn send_text(&self, body: &str) {
		self.send_message(RoomMessageEventContent::text_markdown(body))
			.await
			.ok();
	}

	/// Sends a message to the admin room as the admin user (see send_text() for
	/// convenience).
	async fn send_message(&self, message_content: RoomMessageEventContent) -> Result {
		let user_id = &self.services.globals.server_user;
		let room_id = self.get_admin_room().await?;
		let state_lock = self.services.state.mutex.lock(&room_id).await;

		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder::timeline(&message_content),
				user_id,
				&room_id,
				&state_lock,
			)
			.boxed()
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
		let verdict = self
			.is_admin_command(room_id, sender, command)
			.await;

		if matches!(verdict, AdminCommandCheckVerdict::NotAdminCommand) {
			return;
		}

		let reply_sender = match verdict {
			| AdminCommandCheckVerdict::AdminEscapeCommand => sender,
			| AdminCommandCheckVerdict::AdminRoomCommand => &self.services.globals.server_user,
			| AdminCommandCheckVerdict::NotAdminCommand => unreachable!(),
		};

		let command = command
			.trim_start_matches('\\')
			.trim_start_matches('!');

		self.services.command.run_command_matrix_detached(
			self.get_admin_command_system(),
			event_id,
			room_id,
			command,
			sender,
			reply_sender,
			Some(self.get_capture_level()),
		);
	}

	pub fn set_admin_command_system(&self, command_system: Arc<dyn CommandSystem>) {
		self.admin_command_system
			.set(command_system)
			.ok()
			.expect("admin command system already initialized");
	}

	fn get_admin_command_system(&self) -> &Arc<dyn CommandSystem> {
		self.admin_command_system
			.get()
			.expect("admin command system empty")
	}

	fn get_capture_level(&self) -> Level {
		Level::from_str(&self.services.server.config.admin_log_capture).unwrap_or_else(|e| {
			warn!("admin_log_capture filter invalid: {e:?}");
			if cfg!(debug_assertions) {
				Level::DEBUG
			} else {
				Level::INFO
			}
		})
	}

	pub async fn run_command(&self, command: &str, input: &str) -> CommandResult {
		self.services
			.command
			.run_command(self.get_admin_command_system().as_ref(), command, input, None)
			.await
	}

	pub fn complete_command(&self, command: &str) -> String {
		self.services
			.command
			.complete_command(self.get_admin_command_system().as_ref(), command)
	}

	/// Checks whether a given user is an admin of this server
	pub async fn user_is_admin(&self, user_id: &UserId) -> bool {
		let Ok(admin_room) = self.get_admin_room().await else {
			return false;
		};

		self.services
			.state_cache
			.is_joined(user_id, &admin_room)
			.await
	}

	/// Gets the room ID of the admin room
	///
	/// Errors are propagated from the database, and will have None if there is
	/// no admin room
	pub async fn get_admin_room(&self) -> Result<OwnedRoomId> {
		let room_id = self
			.services
			.alias
			.resolve_local_alias(&self.admin_alias)
			.await?;

		self.services
			.state_cache
			.is_joined(&self.services.globals.server_user, &room_id)
			.await
			.then_some(room_id)
			.ok_or_else(|| err!(Request(NotFound("Admin user not joined to admin room"))))
	}

	async fn is_admin_command(
		&self,
		room_id: &RoomId,
		sender: &UserId,
		body: &str,
	) -> AdminCommandCheckVerdict {
		if !self.user_is_admin(sender).await {
			return AdminCommandCheckVerdict::NotAdminCommand;
		}

		// Server-side command-escape with public echo
		let is_public_escape = body.starts_with('\\')
			&& body
				.trim_start_matches('\\')
				.starts_with("!admin");

		let user_is_local = self.services.globals.user_is_local(sender);

		// Admin command with public echo (in admin room)
		let server_user = &self.services.globals.server_user;
		let is_prefix = body.starts_with("!admin") || body.starts_with(server_user.as_str());

		let is_in_admin_room = self.is_admin_room(room_id).await;

		if self.services.server.config.admin_escape_commands
			&& is_public_escape
			&& user_is_local
			&& !is_in_admin_room
		{
			return AdminCommandCheckVerdict::AdminEscapeCommand;
		}

		let server_user_sender = sender == server_user;

		let emergency_password_set = self
			.services
			.server
			.config
			.emergency_password
			.is_some();

		if is_prefix && is_in_admin_room && (!server_user_sender || !emergency_password_set) {
			return AdminCommandCheckVerdict::AdminRoomCommand;
		}

		AdminCommandCheckVerdict::NotAdminCommand
	}

	#[must_use]
	pub async fn is_admin_room(&self, room_id: &RoomId) -> bool {
		match self.get_admin_room().await {
			| Ok(admin_room) => admin_room == room_id,
			| Err(_) => false,
		}
	}
}
