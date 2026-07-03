pub mod console;
pub mod context;
pub mod create;
mod execute;
mod grant;
mod processor;
mod register;

use std::{
	collections::BTreeMap,
	sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock},
	time::Instant,
};

use async_trait::async_trait;
pub use context::Context;
pub use create::create_admin_room;
use futures::{FutureExt, TryFutureExt};
use ruma::{
	OwnedEventId, OwnedRoomAliasId, OwnedRoomId, RoomId, RoomOrAliasId, UserId,
	events::room::message::{Relation, RoomMessageEventContent},
};
use tokio::sync::mpsc;
use tuwunel_core::{
	Err, Error, Event, Result, debug, err, error, error::default_log, pdu::PduBuilder, warn,
};

use crate::rooms::state::RoomMutexGuard;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	channel: StdRwLock<Option<mpsc::Sender<CommandInput>>>,
	pub command: StdRwLock<Option<Arc<dyn Command>>>,
	pub admin_alias: OwnedRoomAliasId,
	/// Resolved Synapse-compatible registration shared secret. Live for the
	/// lifetime of the service; the matching nonce store sits beside it.
	register_shared_secret: Option<String>,
	register_nonces: StdMutex<BTreeMap<String, Instant>>,
	#[cfg(feature = "console")]
	pub console: Arc<console::Console>,
}

/// Inputs to a command are a multi-line string and optional reply_id.
#[derive(Clone, Debug, Default)]
pub struct CommandInput {
	pub command: String,
	pub reply_id: Option<OwnedEventId>,
}

/// Root of a clap command tree installed by a downstream crate.
#[async_trait]
pub trait Command: Send + Sync + 'static {
	/// The clap command tree; equivalent to
	/// `<C as clap::CommandFactory>::command()`.
	fn clap(&self) -> clap::Command;

	/// Dispatch already-parsed argument matches to the matching handler.
	async fn dispatch(&self, matches: clap::ArgMatches, context: &Context<'_>) -> Result;
}

/// Result wrapping of a command's handling. Both variants are complete message
/// events which have digested any prior errors. The wrapping preserves whether
/// the command failed without interpreting the text. Ok(None) outputs are
/// dropped to produce no response.
pub type ProcessorResult = Result<Option<CommandOutput>, Box<CommandOutput>>;

/// Alias for the output structure.
pub type CommandOutput = RoomMessageEventContent;

/// Maximum number of commands which can be queued for dispatch.
const COMMAND_QUEUE_LIMIT: usize = 512;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			channel: StdRwLock::new(None),
			command: StdRwLock::new(None),
			admin_alias: OwnedRoomAliasId::try_from(format!("#admins:{}", args.server.name))
				.expect("#admins:server_name is valid alias name"),
			register_shared_secret: register::resolve_shared_secret(&args.server.config),
			register_nonces: StdMutex::new(BTreeMap::new()),
			#[cfg(feature = "console")]
			console: console::Console::new(args),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		let mut signals = self.services.server.signal.subscribe();
		let (sender, mut receiver) = mpsc::channel(COMMAND_QUEUE_LIMIT);
		_ = self
			.channel
			.write()
			.expect("locked for writing")
			.insert(sender);

		self.console_auto_start().await;

		loop {
			tokio::select! {
				command = receiver.recv() => match command {
					Some(command) => self.handle_command(command).await,
					None => break,
				},
				sig = signals.recv() => match sig {
					Ok(sig) => self.handle_signal(sig).await,
					Err(_) => continue,
				},
			}
		}

		//TODO: not unwind safe
		self.interrupt().await;
		self.console_auto_stop().await;

		Ok(())
	}

	async fn interrupt(&self) {
		#[cfg(feature = "console")]
		self.console.interrupt();

		_ = self
			.channel
			.write()
			.expect("locked for writing")
			.take();
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
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

	/// Sends a notice gated on the admin_room_notices config; admin command
	/// responses use the unconditional notice().
	pub async fn notify(&self, body: &str) {
		if self.services.server.config.admin_room_notices {
			self.notice(body).await;
		}
	}

	/// Sends a message gated on the admin_room_notices config; admin command
	/// responses use the unconditional send_text().
	pub async fn notify_loud(&self, body: &str) {
		if self.services.server.config.admin_room_notices {
			self.send_text(body).await;
		}
	}

	/// Sends a message to the admin room as the admin user (see send_text() for
	/// convenience).
	pub async fn send_message(&self, message_content: RoomMessageEventContent) -> Result {
		let room_id = self.get_admin_room().await?;

		self.send_to_room(message_content, &room_id).await
	}

	/// Sends a markdown report to the configured report room, falling back to
	/// the admin room, as the server user.
	pub async fn send_report(&self, body: &str) {
		let Ok(room_id) = self.get_report_room().await else {
			return;
		};

		self.send_to_room(RoomMessageEventContent::text_markdown(body), &room_id)
			.await
			.ok();
	}

	async fn send_to_room(&self, content: RoomMessageEventContent, room_id: &RoomId) -> Result {
		let user_id = &self.services.globals.server_user;

		self.respond_to_room(content, room_id, user_id)
			.boxed()
			.await
	}

	/// Posts a command to the command processor queue and returns. Processing
	/// will take place on the service worker's task asynchronously. Errors if
	/// the queue is full.
	pub async fn command(&self, command: String, reply_id: Option<OwnedEventId>) -> Result {
		let Some(sender) = self
			.channel
			.read()
			.expect("locked for reading")
			.clone()
		else {
			return Err!("Admin command queue unavailable.");
		};

		sender
			.send(CommandInput { command, reply_id })
			.await
			.map_err(|e| err!("Failed to enqueue admin command: {e:?}"))
	}

	/// Dispatches a command to the processor on the current task and waits for
	/// completion.
	pub async fn command_in_place(
		&self,
		command: String,
		reply_id: Option<OwnedEventId>,
	) -> ProcessorResult {
		self.process_command(CommandInput { command, reply_id })
			.await
	}

	/// Invokes the tab-completer to complete the command. When unavailable,
	/// None is returned.
	pub fn complete_command(&self, command: &str) -> Option<String> {
		self.command
			.read()
			.expect("locked for reading")
			.as_ref()
			.map(|root| processor::complete(root.clap(), command))
	}

	async fn handle_signal(&self, sig: &'static str) {
		if sig == execute::SIGNAL {
			self.signal_execute().await.ok();
		}

		#[cfg(feature = "console")]
		self.console.handle_signal(sig);
	}

	async fn handle_command(&self, command: CommandInput) {
		match self.process_command(command).await {
			| Err(output) => self.handle_command_output(*output).await,
			| Ok(Some(output)) => self.handle_command_output(output).await,
			| Ok(None) => debug!("Command successful with no response"),
		}
	}

	async fn handle_command_output(&self, content: RoomMessageEventContent) {
		self.handle_response(content)
			.await
			.unwrap_or_else(default_log);
	}

	async fn process_command(&self, command: CommandInput) -> ProcessorResult {
		let root = self
			.command
			.read()
			.expect("locked for reading")
			.clone()
			.expect("Admin module is not loaded");

		processor::handle_command(root, Arc::clone(self.services.get()), command).await
	}

	/// Checks whether a given user is an admin of this server
	pub async fn user_is_admin(&self, user_id: &UserId) -> bool {
		if user_id == self.services.globals.server_user {
			return true;
		}

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

	/// Gets the room reports are posted to: the configured report room when set
	/// and usable, otherwise the admin room.
	pub async fn get_report_room(&self) -> Result<OwnedRoomId> {
		let Some(report_room) = self.services.server.config.report_room.as_ref() else {
			return self.get_admin_room().await;
		};

		match self.resolve_report_room(report_room).await {
			| Ok(room_id) => Ok(room_id),
			| Err(e) => {
				warn!(%report_room, error = %e, "Falling back to the admin room for reports");
				self.get_admin_room().await
			},
		}
	}

	async fn resolve_report_room(&self, report_room: &RoomOrAliasId) -> Result<OwnedRoomId> {
		let room_id = self
			.services
			.alias
			.maybe_resolve(report_room)
			.await?;

		self.services
			.state_cache
			.is_joined(&self.services.globals.server_user, &room_id)
			.await
			.then_some(room_id)
			.ok_or_else(|| err!("server user is not joined to the configured report room"))
	}

	async fn handle_response(&self, content: RoomMessageEventContent) -> Result {
		let Some(Relation::Reply(ruma::events::relation::Reply { in_reply_to })) =
			content.relates_to.as_ref()
		else {
			return Ok(());
		};

		let Ok(pdu) = self
			.services
			.timeline
			.get_pdu(&in_reply_to.event_id)
			.await
		else {
			error!(
				event_id = ?in_reply_to.event_id,
				"Missing admin command in_reply_to event"
			);
			return Ok(());
		};

		let response_sender = if self.is_admin_room(pdu.room_id()).await {
			&self.services.globals.server_user
		} else {
			pdu.sender()
		};

		self.respond_to_room(content, pdu.room_id(), response_sender)
			.boxed()
			.await
	}

	async fn respond_to_room(
		&self,
		content: RoomMessageEventContent,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result {
		assert!(self.user_is_admin(user_id).await, "sender is not admin");

		let state_lock = self.services.state.mutex.lock(room_id).await;

		if let Err(e) = self
			.services
			.timeline
			.build_and_append_pdu(PduBuilder::timeline(&content), user_id, room_id, &state_lock)
			.await
		{
			self.handle_response_error(e, room_id, user_id, &state_lock)
				.boxed()
				.await
				.unwrap_or_else(default_log);
		}

		Ok(())
	}

	async fn handle_response_error(
		&self,
		e: Error,
		room_id: &RoomId,
		user_id: &UserId,
		state_lock: &RoomMutexGuard,
	) -> Result {
		error!("Failed to build and append admin room response PDU: \"{e}\"");
		let content = RoomMessageEventContent::text_plain(format!(
			"Failed to build and append admin room PDU: \"{e}\"\n\nThe original admin command \
			 may have finished successfully, but we could not return the output."
		));

		self.services
			.timeline
			.build_and_append_pdu(PduBuilder::timeline(&content), user_id, room_id, state_lock)
			.boxed()
			.await?;

		Ok(())
	}

	pub async fn is_admin_command<Pdu>(&self, event: &Pdu, body: &str) -> bool
	where
		Pdu: Event,
	{
		// Server-side command-escape with public echo
		let is_escape = body.starts_with('\\');
		let is_public_escape = is_escape
			&& body
				.trim_start_matches('\\')
				.starts_with("!admin");

		// Admin command with public echo (in admin room)
		let server_user = &self.services.globals.server_user;
		let is_public_prefix =
			body.starts_with("!admin") || body.starts_with(server_user.as_str());

		// Expected backward branch
		if !is_public_escape && !is_public_prefix {
			return false;
		}

		let user_is_local = self
			.services
			.globals
			.user_is_local(event.sender());

		// only allow public escaped commands by local admins
		if is_public_escape && !user_is_local {
			return false;
		}

		// Check if server-side command-escape is disabled by configuration
		if is_public_escape && !self.services.server.config.admin_escape_commands {
			return false;
		}

		// Prevent unescaped !admin from being used outside of the admin room
		if is_public_prefix && !self.is_admin_room(event.room_id()).await {
			return false;
		}

		// Only senders who are admin can proceed
		if !self.user_is_admin(event.sender()).await {
			return false;
		}

		// This will evaluate to false if the emergency password is set up so that
		// the administrator can execute commands as the server user
		let emergency_password_set = self
			.services
			.server
			.config
			.emergency_password
			.is_some();
		let from_server = event.sender() == server_user && !emergency_password_set;
		if from_server && self.is_admin_room(event.room_id()).await {
			return false;
		}

		// Authentic admin command
		true
	}

	#[must_use]
	pub async fn is_admin_room(&self, room_id_: &RoomId) -> bool {
		self.get_admin_room()
			.map_ok(|room_id| room_id == room_id_)
			.await
			.unwrap_or(false)
	}
}
