//! Admin-command assertions shared by the server-booting tests that opt in.
//!
//! Each runs one command on the processor in place and checks its verdict, so
//! a test states the outcome it expects rather than inspecting the output
//! itself.

use tuwunel_core::{Err, Result, err};
use tuwunel_service::Services;

/// Runs `command` and requires it to fail with `reason` in its output.
///
/// Matching the reason keeps a parse error or a renamed command from passing
/// as the refusal under test.
pub(crate) async fn refused(services: &Services, command: &str, reason: &str) -> Result {
	match services
		.admin
		.command_in_place(command.into(), None)
		.await
	{
		| Err(output) if output.as_str().contains(reason) => Ok(()),
		| Ok(None) => Err!("{command:?} succeeded without output"),
		| Ok(Some(output)) | Err(output) => {
			let output = output.as_str();

			Err!("{command:?} was not refused for {reason:?}: {output}")
		},
	}
}

/// Runs `command` and requires it to succeed.
///
/// Any success passes, with or without output; a refusal carries the
/// command's error output into the test failure.
pub(crate) async fn accepted(services: &Services, command: &str) -> Result {
	services
		.admin
		.command_in_place(command.into(), None)
		.await
		.map(drop)
		.map_err(|output| {
			let output = output.as_str();

			err!("{command:?} was refused: {output}")
		})
}
