use std::{panic::AssertUnwindSafe, time::SystemTime};

use futures::FutureExt;
use ruma::UserId;
use tracing::{Instrument, Level, span};
use tuwunel_core::{Error, debug, error};

use super::{CommandResult, CommandSystem, Service};

impl Service {
	pub async fn run_command(
		&self,
		command_system: &dyn CommandSystem,
		line: &str,
		input: &str,
		sender: Option<&UserId>,
	) -> CommandResult {
		let timer = SystemTime::now();

		let span = span!(Level::INFO, "command");
		let span_id = span.id();
		// start capture if the span is enabled
		if let Some(ref span_id) = span_id {
			self.services
				.server
				.log
				.capture
				.start_capture(span_id);
		}

		let args = command_system.parse(line);

		let args = args
			.iter()
			.map(String::as_str)
			.collect::<Vec<&str>>();

		let result = AssertUnwindSafe(Box::pin(
			command_system
				.process(&args, input, sender)
				.instrument(span),
		))
		.catch_unwind()
		.await
		.map(|result| result.map_err(|error| format!("Command failed: \n{error}")))
		.unwrap_or_else(|panic| {
			let error = Error::from_panic(panic);
			error!("Panic while processing command: {error:?}");
			Err(format!(
				"Panic occurred while processing command:\n\
						```\n\
						{error:#?}\n\
						```\n\
						Please submit a [bug report](https://github.com/matrix-construct/tuwunel/issues/new).🥺"
			))
		});

		let (output, err) = match result {
			| Ok(output) => (output, false),
			| Err(output) => (output, true),
		};

		// return logs if span is active, otherwise empty
		let logs = if let Some(ref span_id) = span_id {
			self.services
				.server
				.log
				.capture
				.stop_capture(span_id)
		} else {
			Vec::new()
		};

		debug!(
			ok = !err,
			elapsed = ?timer.elapsed(),
			command = ?args,
			"command processed"
		);

		CommandResult { output, logs, err }
	}
}
