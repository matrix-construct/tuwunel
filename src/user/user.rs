use clap::Parser;
use tuwunel_core::Result;
use tuwunel_macros::command_dispatch;

use crate::user::{debug::Cmd as DebugCmd, retention::Cmd as RetentionCmd};

#[derive(Debug, Parser)]
#[command(name = "user", version = tuwunel_core::version())]
#[command(
	arg_required_else_help = true,
	subcommand_required = true,
	subcommand_value_name = "COMMAND"
)]
#[command_dispatch]
pub(super) enum UserCommand {
	#[command(subcommand)]
	/// Debugging and diagnostic commands
	Debug(DebugCmd),
	#[command(subcommand)]
	/// Media retention and auto-delete preferences
	Retention(RetentionCmd),
}

mod debug {
	use clap::Subcommand;
	use tuwunel_core::Result;
	use tuwunel_macros::{command, command_dispatch};

	#[command_dispatch]
	#[derive(Debug, Subcommand)]
	pub(crate) enum Cmd {
		/// Echo test command
		Echo {},
	}

	#[command]
	pub(super) async fn echo(&self) -> Result<String> {
		let sender = self.sender;
		Ok(format!("Running echo command from {sender}"))
	}
}

mod retention {
	use clap::Subcommand;
	use tuwunel_core::Result;
	use tuwunel_macros::{command, command_dispatch};

	#[command_dispatch]
	#[derive(Debug, Subcommand)]
	pub(crate) enum Cmd {
		Confirm {
			mxc: String,
		},
		/// Show current auto-delete preferences
		PrefsShow,
		/// Enable auto-delete for encrypted rooms
		PrefsEncryptedOn,
		/// Disable auto-delete for encrypted rooms
		PrefsEncryptedOff,
		/// Enable auto-delete for unencrypted rooms
		PrefsUnencryptedOn,
		/// Disable auto-delete for unencrypted rooms
		PrefsUnencryptedOff,
		/// Reset all preferences (disable auto-delete for both)
		PrefsReset,
	}

	#[command]
	pub(super) async fn confirm(&self, mxc: String) -> Result<String> {
		let bytes = self
			.services
			.media
			.retention_confirm_deletion(self.sender, &mxc)
			.await?;

		let summary = if bytes > 0 {
			format!("Removed {bytes} bytes of local media.")
		} else {
			"No local media files were found to delete.".to_owned()
		};

		Ok(format!("Confirmed deletion for {mxc}. {summary}"))
	}

	#[command]
	pub(super) async fn prefs_show(&self) -> Result<String> {
		let prefs = self
			.services
			.media
			.retention
			.get_user_prefs(self.sender.as_str())
			.await;

		Ok(format!(
			"Current auto-delete preferences:\n- Encrypted rooms: {}\n- Unencrypted rooms: {}",
			if prefs.auto_delete_encrypted {
				"enabled ✅"
			} else {
				"disabled ❌"
			},
			if prefs.auto_delete_unencrypted {
				"enabled ✅"
			} else {
				"disabled ❌"
			}
		))
	}

	#[command]
	pub(super) async fn prefs_encrypted_on(&self) -> Result<String> {
		let mut prefs = self
			.services
			.media
			.retention
			.get_user_prefs(self.sender.as_str())
			.await;

		prefs.auto_delete_encrypted = true;

		self.services
			.media
			.retention
			.set_user_prefs(self.sender.as_str(), &prefs)
			.await?;

		Ok("Enabled auto-delete for encrypted rooms.".to_owned())
	}

	#[command]
	pub(super) async fn prefs_encrypted_off(&self) -> Result<String> {
		let mut prefs = self
			.services
			.media
			.retention
			.get_user_prefs(self.sender.as_str())
			.await;

		prefs.auto_delete_encrypted = false;

		self.services
			.media
			.retention
			.set_user_prefs(self.sender.as_str(), &prefs)
			.await?;

		Ok("Disabled auto-delete for encrypted rooms.".to_owned())
	}

	#[command]
	pub(super) async fn prefs_unencrypted_on(&self) -> Result<String> {
		let mut prefs = self
			.services
			.media
			.retention
			.get_user_prefs(self.sender.as_str())
			.await;

		prefs.auto_delete_unencrypted = true;

		self.services
			.media
			.retention
			.set_user_prefs(self.sender.as_str(), &prefs)
			.await?;

		Ok("Enabled auto-delete for unencrypted rooms.".to_owned())
	}

	#[command]
	pub(super) async fn prefs_unencrypted_off(&self) -> Result<String> {
		let mut prefs = self
			.services
			.media
			.retention
			.get_user_prefs(self.sender.as_str())
			.await;

		prefs.auto_delete_unencrypted = false;

		self.services
			.media
			.retention
			.set_user_prefs(self.sender.as_str(), &prefs)
			.await?;

		Ok("Disabled auto-delete for unencrypted rooms.".to_owned())
	}

	#[command]
	pub(super) async fn prefs_reset(&self) -> Result<String> {
		let prefs = Default::default();

		self.services
			.media
			.retention
			.set_user_prefs(self.sender.as_str(), &prefs)
			.await?;

		Ok("Reset all auto-delete preferences. All auto-delete settings disabled.".to_owned())
	}
}
