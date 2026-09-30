mod attach;
pub mod console;
pub mod context;
pub mod create;
mod execute;
mod grant;
mod notices;
mod processor;
mod register;
mod respond;

use std::{
	collections::BTreeMap,
	sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock},
	time::Instant,
};

use async_trait::async_trait;
pub use context::Context;
pub use create::create_admin_room;
use futures::TryFutureExt;
use ruma::{
	OwnedEventId, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId, RoomOrAliasId, UserId,
};
use tokio::sync::mpsc;
use tuwunel_core::{
	Err, Event, Result, debug, err, error::default_log, implement, matrix::event::MsgType,
	utils::ReadyExt, warn,
};

use crate::rooms::state::RoomMutexGuard;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	channel: StdRwLock<Option<mpsc::Sender<CommandInput>>>,
	pub command: StdRwLock<Option<Arc<dyn Command>>>,
	pub admin_alias: OwnedRoomAliasId,
	register_nonces: StdMutex<BTreeMap<String, Instant>>,
	#[cfg(feature = "console")]
	pub console: Arc<console::Console>,
}

/// Inputs to a command: its multi-line text, the event to reply to, and who
/// sent it.
///
/// An input without a sender is the operator's, from the console or the
/// `admin_execute` and `admin_signal_execute` lists; converting a bare command
/// string builds one.
#[derive(Clone, Debug, Default)]
pub struct CommandInput {
	/// The command line, followed by any body lines.
	pub command: String,

	/// The event the command's response replies to.
	pub reply_id: Option<OwnedEventId>,

	/// The user who sent the command, or `None` for the operator.
	pub sender: Option<OwnedUserId>,
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

/// Carries a rendered command outcome while preserving its status.
///
/// `Ok(Some(output))` reports success, `Err(output)` reports failure, and
/// `Ok(None)` suppresses the response. Callers do not infer status from text.
pub type ProcessorResult = Result<Option<CommandOutput>, CommandOutput>;

/// Textual output of a completed command. Markdown is the norm; Plain carries
/// clap usage and error text, which must never be markdown-rendered.
pub enum CommandOutput {
	Markdown(String),
	Plain(String),
}

impl From<String> for CommandInput {
	fn from(command: String) -> Self { Self { command, ..Default::default() } }
}

impl From<&str> for CommandInput {
	fn from(command: &str) -> Self { command.to_owned().into() }
}

impl CommandOutput {
	#[inline]
	#[must_use]
	pub fn as_str(&self) -> &str {
		match self {
			| Self::Markdown(text) | Self::Plain(text) => text,
		}
	}
}

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
				sig = signals.recv() => if let Ok(sig) = sig {
					self.handle_signal(sig).await;
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
	/// Queues a command for the service worker and returns once it is queued.
	///
	/// The worker processes it later and replies to the `reply_id` event in its
	/// room, posting nothing without one. Queueing waits while the queue is
	/// full, and errors when the queue is unavailable or closed.
	pub async fn command(&self, input: CommandInput) -> Result {
		let Some(queue) = self
			.channel
			.read()
			.expect("locked for reading")
			.clone()
		else {
			return Err!("Admin command queue unavailable.");
		};

		queue
			.send(input)
			.map_err(|e| err!("Failed to enqueue admin command: {e:?}"))
			.await
	}

	/// Dispatches a command to the processor on the current task and waits for
	/// completion.
	///
	/// The queue is bypassed, so the outcome returns to the caller rather than
	/// being posted as a reply.
	pub async fn command_in_place(&self, input: CommandInput) -> ProcessorResult {
		self.process_command(&input).await
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
		match self.process_command(&command).await {
			| Ok(None) => debug!("Command successful with no response"),
			| Err(output) | Ok(Some(output)) => self
				.handle_response(output, command.reply_id.as_deref())
				.await
				.unwrap_or_else(default_log),
		}
	}

	async fn process_command(&self, command: &CommandInput) -> ProcessorResult {
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

	/// Checks whether a given user is the only active admin left on this server.
	///
	/// The server user is never counted: it can sign in only while an emergency
	/// password is configured. Deactivated accounts still joined to the admin
	/// room are not counted either, since none of them can sign in to act. Nor
	/// is a passwordless account, such as an appservice's user, since it stores
	/// the same empty password as a deactivated one.
	pub async fn user_is_last_admin(&self, user_id: &UserId) -> bool {
		let server_user: &UserId = &self.services.globals.server_user;
		if user_id == server_user {
			return false;
		}

		let Ok(admin_room) = self.get_admin_room().await else {
			return false;
		};

		if !self
			.services
			.state_cache
			.is_joined(user_id, &admin_room)
			.await
		{
			return false;
		}

		!self
			.services
			.state_cache
			.active_local_users_in_room(&admin_room)
			.ready_any(|member| member != user_id && member != server_user)
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

	/// Returns whether a message event is an admin command to run.
	///
	/// Only an `m.text` from an admin qualifies: prefixed with `!admin` or the
	/// server user's ID in the admin room, or escaped as `\!admin` by a local
	/// admin anywhere when escape commands are enabled. The server user's own
	/// messages in the admin room are refused unless the emergency password is
	/// set.
	pub async fn is_admin_command<Pdu>(&self, event: &Pdu, body: &str) -> bool
	where
		Pdu: Event,
	{
		let body = body.trim_start();

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

		// Spec: an m.notice must never be answered automatically. Other msgtypes'
		// bodies are captions, filenames or emotes a forward or repost can carry.
		if event.msgtype() != Some(MsgType::Text) {
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

/// Locks the admins room's state, when there is an admins room.
///
/// Hold the guard from the [`Service::user_is_last_admin`] check until the
/// change it permits is made. The admins room's leave and ban guard runs under
/// the same lock, so two concurrent removals cannot each see the other as the
/// admin who remains.
#[implement(Service)]
pub async fn lock_admin_room(&self) -> Option<RoomMutexGuard> {
	let admin_room = self.get_admin_room().await.ok()?;

	self.services
		.state
		.mutex
		.lock(&admin_room)
		.await
		.into()
}
