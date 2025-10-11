use clap::Parser;
use tuwunel_core::Result;
use tuwunel_macros::{command, command_dispatch};

use crate::user::{debug::Cmd as DebugCmd, retention::Cmd as RetentionCmd};

#[derive(Debug, Parser)]
#[command(name = "tuwunel", version = tuwunel_core::version())]
#[command_dispatch]
pub(super) enum UserCommand {
	#[command(subcommand)]
	Debug(DebugCmd),
	#[command(subcommand)]
	Retention(RetentionCmd),
}

mod debug {
	use clap::Subcommand;
	use tuwunel_core::Result;
	use tuwunel_macros::{command, command_dispatch};

	#[command_dispatch]
	#[derive(Debug, Subcommand)]
	pub(crate) enum Cmd {
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
		Confirm { mxc: String },
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
}
