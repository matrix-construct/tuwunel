//! Configures server administration and command output.
//!
//! Startup and signal commands share administration policy with interactive
//! commands. Room settings and the durable server-user identity govern
//! administrative communication.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::{OwnedRoomOrAliasId, ServerUserLocalpart, true_fn};

/// Configures server administration and command output.
///
/// Startup and signal commands share administration policy with interactive
/// commands. Room settings and the durable server-user identity govern
/// administrative communication.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.admin",
	group = "true",
	ignore = "room"
)]
pub struct Admin {
	/// Allows admins to run commands outside the admin room by prefixing
	/// messages with "\!admin" or "\\!admin".
	///
	/// Follow the prefix with a normal command, as in the "#admins" room; the
	/// reply is publicly visible in the current room and originates from the
	/// sender. This is disabled by default because we cannot distinguish a
	/// typed command from a forwarded message or an account bot's repost.
	/// Message formatting can also hide a command from the admin.
	///
	/// reloadable: yes
	/// example: \\!admin debug ping puppygock.gay
	/// legacy: admin_escape_commands
	#[serde(default)]
	pub escape_commands: bool,

	/// Activates the admin console automatically at startup.
	///
	/// The console command-line argument also enables it. Activation requires
	/// standard input to be a terminal.
	///
	/// legacy: admin_console_automatic
	#[serde(default)]
	pub console_automatic: bool,

	#[expect(clippy::doc_link_with_quotes)]
	/// Lists admin commands to execute at startup.
	///
	/// The execute command-line argument also configures these commands and
	/// supports shell commands and environment-variable expansion. A shell
	/// can, for example, supply `server admin-notice tuwunel has started up at
	/// $(date)` as the execute argument.
	///
	/// example: ["debug ping puppygock.gay", "debug echo hi"]
	///
	/// default: []
	/// legacy: admin_execute
	#[serde(default)]
	pub execute: Vec<String>,

	/// Ignores failures in startup admin commands.
	///
	/// When false, a failing command supplied by the execute command-line
	/// argument or `admin.execute` prevents startup.
	///
	/// reloadable: yes
	/// legacy: admin_execute_errors_ignore
	#[serde(default)]
	pub execute_errors_ignore: bool,

	/// Lists admin commands to execute on SIGUSR2.
	///
	/// This is like `admin.execute`, but runs when the signal arrives on
	/// supporting platforms.
	///
	/// reloadable: yes
	/// default: []
	/// legacy: admin_signal_execute
	#[serde(default)]
	pub signal_execute: Vec<String>,

	/// Sets the maximum log level captured during admin commands.
	///
	/// The default is info for release builds and debug for debug builds.
	///
	/// reloadable: yes
	/// default: "info"
	/// legacy: admin_log_capture
	#[serde(default = "default_log_capture")]
	pub log_capture: String,

	/// Sets the localpart of the server's administrative user.
	///
	/// The identity becomes durable after first boot. Changing it on an
	/// existing database is unsupported and prevents startup.
	///
	/// default: "conduit"
	/// legacy: server_user_localpart
	#[serde(default = "default_server_user_localpart")]
	pub server_user_localpart: ServerUserLocalpart,

	/// Selects a room for user, room, and event reports instead of the admin
	/// room.
	///
	/// Accepts a room ID or alias, and the server user must be joined with
	/// permission to post. Reports fall back to the admin room when unset,
	/// unresolvable, or the server user is not a member.
	///
	/// reloadable: yes
	/// default: (none)
	/// legacy: report_room
	#[serde(default)]
	pub report_room: Option<OwnedRoomOrAliasId>,

	/// Limits message events used for an admin command's output replies.
	///
	/// Output requiring more events is uploaded to the media repository and
	/// returned as a text attachment replying to the command. With 1, output
	/// fitting one event is a single reply and larger output becomes an
	/// attachment. With 0, output is always an attachment regardless of size.
	///
	/// reloadable: yes
	/// default: 1
	/// legacy: admin_output_max_events
	#[serde(default = "default_output_max_events")]
	pub output_max_events: usize,

	/// Posts admin-command output in a thread on the command event instead of
	/// replies.
	///
	/// Output split across events by `admin.output_max_events` stays in one
	/// thread. Attachments are also posted in that thread.
	///
	/// reloadable: yes
	/// legacy: admin_output_threads
	#[serde(default)]
	pub output_threads: bool,

	/// Settings for admin room.
	///
	/// display: section
	#[serde(default)]
	pub room: Room,
}

/// Configures the administration room.
///
/// Creation and federation settings determine its initial setup. Tags and
/// notice policies control its presentation and routine messages.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.admin.room",
	group = "true"
)]
pub struct Room {
	/// Logs guest registrations in the admin room, which may be noisy or
	/// unnecessary on a public server.
	///
	/// reloadable: yes
	/// legacy: log_guest_registrations
	#[serde(default)]
	pub log_guest_registrations: bool,

	/// Sets the default tag applied to the admin room, with `m.server_notice`
	/// providing a dedicated, easily accessed shortcut pinned at the bottom of
	/// the room list in clients such as Element.
	///
	/// reloadable: yes
	/// default: "m.server_notice"
	/// legacy: admin_room_tag
	#[serde(default = "default_tag")]
	pub tag: String,

	/// Creates the admin room on first startup and should remain enabled for
	/// users, though developers can disable it in integration tests to reduce
	/// activity and output.
	///
	/// default: true
	/// legacy: create_admin_room
	#[serde(default = "true_fn")]
	pub create: bool,

	/// Enables federation for the admin room.
	///
	/// This cannot be changed after the room is created.
	///
	/// default: true
	/// legacy: federate_admin_room
	#[serde(default = "true_fn")]
	pub federate: bool,

	/// Sends administrative notices to the admin room.
	///
	/// These include registrations, password changes, account deactivations,
	/// and room-directory publications. Update notices and ordinary
	/// admin-command responses are sent regardless.
	///
	/// reloadable: yes
	/// legacy: admin_room_notices
	#[serde(default = "true_fn")]
	pub notices: bool,
}

impl Default for Admin {
	fn default() -> Self {
		Self {
			escape_commands: Default::default(),
			console_automatic: Default::default(),
			execute: Default::default(),
			execute_errors_ignore: Default::default(),
			signal_execute: Default::default(),
			log_capture: default_log_capture(),
			server_user_localpart: default_server_user_localpart(),
			report_room: Default::default(),
			output_max_events: default_output_max_events(),
			output_threads: Default::default(),
			room: Room::default(),
		}
	}
}

impl Default for Room {
	fn default() -> Self {
		Self {
			log_guest_registrations: Default::default(),
			tag: default_tag(),
			create: true_fn(),
			federate: true_fn(),
			notices: true_fn(),
		}
	}
}

fn default_log_capture() -> String {
	cfg!(debug_assertions)
		.then_some("debug")
		.unwrap_or("info")
		.to_owned()
}

/// Preserves the administrative identity used before it was configurable.
///
/// Omitting the setting keeps existing installations on their original user.
fn default_server_user_localpart() -> ServerUserLocalpart { "conduit".into() }

fn default_output_max_events() -> usize { 1 }

fn default_tag() -> String { "m.server_notice".to_owned() }
