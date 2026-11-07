//! Reads back the reply of an admin command a server-booting test runs.
//!
//! Only the binaries that inspect a command's output include it, by path, so the
//! others sharing the admin assertions stay free of its dead code.

use tuwunel_core::{Err, Result};
use tuwunel_service::{Services, admin::CommandOutput};

/// Runs `command` on the processor in place and returns its output.
///
/// A refusal or a reply with no output fails, naming the command.
pub(crate) async fn replied(services: &Services, command: &str) -> Result<String> {
	match services
		.admin
		.command_in_place(command.into())
		.await
	{
		| Ok(Some(CommandOutput::Markdown(text) | CommandOutput::Plain(text))) => Ok(text),
		| Ok(None) => Err!("{command:?} succeeded without output"),
		| Err(output) => {
			let output = output.as_str();

			Err!("{command:?} was refused: {output}")
		},
	}
}
