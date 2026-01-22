use std::{
	fmt::Write,
	mem::take,
	panic::AssertUnwindSafe,
	sync::{Arc, Mutex},
	time::SystemTime,
};

use clap::{CommandFactory, Parser};
use futures::{AsyncWriteExt, future::FutureExt, io::BufWriter};
use ruma::{
	EventId,
	events::{
		relation::InReplyTo,
		room::message::{Relation::Reply, RoomMessageEventContent},
	},
};
use tracing::Level;
use tracing_subscriber::{EnvFilter, filter::LevelFilter};
use tuwunel_core::{
	Error, Result, debug, error,
	log::{
		capture,
		capture::Capture,
		fmt::{markdown_table, markdown_table_head},
	},
	trace,
	utils::string::{collect_stream, common_prefix},
	warn,
};
use tuwunel_service::{
	Services,
	admin::{CommandInput, CommandOutput, ProcessorFuture, ProcessorResult},
};

use crate::{admin, admin::AdminCommand, context::Context};

#[must_use]
pub(super) fn complete(line: &str) -> String { complete_command(AdminCommand::command(), line) }

#[must_use]
pub(super) fn dispatch(services: Arc<Services>, command: CommandInput) -> ProcessorFuture {
	Box::pin(handle_command(services, command))
}

#[tracing::instrument(skip_all, name = "admin")]
async fn handle_command(services: Arc<Services>, command: CommandInput) -> ProcessorResult {
	AssertUnwindSafe(Box::pin(process_command(services, &command)))
		.catch_unwind()
		.await
		.map_err(Error::from_panic)
		.unwrap_or_else(|error| handle_panic(&error, &command))
}

async fn process_command(services: Arc<Services>, input: &CommandInput) -> ProcessorResult {
	let (command, args, body) = match parse(&services, input) {
		| Err(error) => return Err(Box::new(error)),
		| Ok(parsed) => parsed,
	};

	let context = Context {
		services: &services,
		body: &body,
		timer: SystemTime::now(),
		reply_id: input.reply_id.as_deref(),
		output: BufWriter::new(Vec::new()).into(),
	};

	let (result, mut logs) = process(&context, command, &args).await;

	let output = &mut context.output.lock().await;
	output
		.flush()
		.await
		.expect("final flush of output stream");

	let output =
		String::from_utf8(take(output.get_mut())).expect("invalid utf8 in command output stream");

	match result {
		| Ok(()) if logs.is_empty() =>
			Ok(Some(reply(RoomMessageEventContent::notice_markdown(output), context.reply_id))),

		| Ok(()) => {
			logs.write_str(output.as_str())
				.expect("output buffer");
			Ok(Some(reply(RoomMessageEventContent::notice_markdown(logs), context.reply_id)))
		},
		| Err(error) => {
			write!(&mut logs, "Command failed with error:\n```\n{error:#?}\n```")
				.expect("output buffer");

			Err(Box::new(reply(
				RoomMessageEventContent::notice_markdown(logs),
				context.reply_id,
			)))
		},
	}
}

fn handle_panic(error: &Error, command: &CommandInput) -> ProcessorResult {
	let link =
		"Please submit a [bug report](https://github.com/matrix-construct/tuwunel/issues/new). \
		 🥺";
	let msg = format!("Panic occurred while processing command:\n```\n{error:#?}\n```\n{link}");
	let content = RoomMessageEventContent::notice_markdown(msg);
	error!("Panic while processing command: {error:?}");
	Err(Box::new(reply(content, command.reply_id.as_deref())))
}

/// Parse and process a message from the admin room
async fn process(
	context: &Context<'_>,
	command: AdminCommand,
	args: &[String],
) -> (Result, String) {
	let (capture, logs) = capture_create(context);

	let capture_scope = capture.start();
	let result = Box::pin(admin::process(command, context)).await;
	drop(capture_scope);

	debug!(
		ok = result.is_ok(),
		elapsed = ?context.timer.elapsed(),
		command = ?args,
		"command processed"
	);

	let mut output = String::new();

	// Prepend the logs only if any were captured
	let logs = logs.lock().expect("locked");
	if logs.lines().count() > 2 {
		writeln!(&mut output, "{logs}").expect("failed to format logs to command output");
	}
	drop(logs);

	(result, output)
}

fn capture_create(context: &Context<'_>) -> (Arc<Capture>, Arc<Mutex<String>>) {
	let env_config = &context.services.server.config.admin_log_capture;
	let env_filter = EnvFilter::try_new(env_config).unwrap_or_else(|e| {
		warn!("admin_log_capture filter invalid: {e:?}");
		cfg!(debug_assertions)
			.then_some("debug")
			.or(Some("info"))
			.map(Into::into)
			.expect("default capture EnvFilter")
	});

	let log_level = env_filter
		.max_level_hint()
		.and_then(LevelFilter::into_level)
		.unwrap_or(Level::DEBUG);

	let filter = move |data: capture::Data<'_>| {
		data.level() <= log_level && data.our_modules() && data.scope.contains(&"admin")
	};

	let logs = Arc::new(Mutex::new(
		collect_stream(|s| markdown_table_head(s)).expect("markdown table header"),
	));

	let capture = Capture::new(
		&context.services.server.log.capture,
		Some(filter),
		capture::fmt(markdown_table, logs.clone()),
	);

	(capture, logs)
}

/// Parse chat messages from the admin room into an AdminCommand object
#[expect(clippy::result_large_err)]
fn parse<'a>(
	services: &Arc<Services>,
	input: &'a CommandInput,
) -> Result<(AdminCommand, Vec<String>, Vec<&'a str>), CommandOutput> {
	let lines = input
		.command
		.lines()
		.filter(|line| !line.trim().is_empty());
	let command_line = lines
		.clone()
		.next()
		.expect("command missing first line");
	let body = lines.skip(1).collect();
	match parse_command(command_line) {
		| Ok((command, args)) => Ok((command, args, body)),
		| Err(error) => {
			let message = error
				.to_string()
				.replace("server.name", services.globals.server_name().as_str());
			Err(reply(RoomMessageEventContent::notice_plain(message), input.reply_id.as_deref()))
		},
	}
}

fn parse_command(line: &str) -> Result<(AdminCommand, Vec<String>)> {
	let argv = parse_line(line);
	let command = AdminCommand::try_parse_from(&argv)?;
	Ok((command, argv))
}

fn complete_command(mut cmd: clap::Command, line: &str) -> String {
	let argv = parse_line(line);
	let mut ret = Vec::<String>::with_capacity(argv.len().saturating_add(1));

	'token: for token in argv.into_iter().skip(1) {
		let cmd_ = cmd.clone();
		let mut choice = Vec::new();

		for sub in cmd_.get_subcommands() {
			let name = sub.get_name();
			if *name == token {
				// token already complete; recurse to subcommand
				ret.push(token);
				cmd.clone_from(sub);
				continue 'token;
			} else if name.starts_with(&token) {
				// partial match; add to choices
				choice.push(name);
			}
		}

		if choice.len() == 1 {
			// One choice. Add extra space because it's complete
			let choice = *choice.first().expect("only choice");
			ret.push(choice.to_owned());
			ret.push(String::new());
		} else if choice.is_empty() {
			// Nothing found, return original string
			ret.push(token);
		} else {
			// Find the common prefix
			ret.push(common_prefix(&choice).into());
		}

		// Return from completion
		return ret.join(" ");
	}

	// Return from no completion. Needs a space though.
	ret.push(String::new());
	ret.join(" ")
}

/// Parse chat messages from the admin room into an AdminCommand object
fn parse_line(command_line: &str) -> Vec<String> {
	let mut argv = command_line
		.split_whitespace()
		.map(str::to_owned)
		.collect::<Vec<String>>();

	// Remove any escapes that came with a server-side escape command
	if !argv.is_empty() && argv[0].ends_with("admin") {
		argv[0] = argv[0].trim_start_matches('\\').into();
	}

	// First indice has to be "admin" but for console convenience we add it here
	if !argv.is_empty() && !argv[0].ends_with("admin") && !argv[0].starts_with('@') {
		argv.insert(0, "admin".to_owned());
	}

	// Replace `help command` with `command --help`
	// Clap has a help subcommand, but it omits the long help description.
	if argv.len() > 1 && argv[1] == "help" {
		argv.remove(1);
		argv.push("--help".to_owned());
	}

	// Backwards compatibility with `register_appservice`-style commands
	if argv.len() > 1 && argv[1].contains('_') {
		argv[1] = argv[1].replace('_', "-");
	}

	// Backwards compatibility with `register_appservice`-style commands
	if argv.len() > 2 && argv[2].contains('_') {
		argv[2] = argv[2].replace('_', "-");
	}

	// if the user is using the `query` command (argv[1]), replace the database
	// function/table calls with underscores to match the codebase
	if argv.len() > 3 && argv[1].eq("query") {
		argv[3] = argv[3].replace('_', "-");
	}

	trace!(?command_line, ?argv, "parse");
	argv
}

fn reply(
	mut content: RoomMessageEventContent,
	reply_id: Option<&EventId>,
) -> RoomMessageEventContent {
	content.relates_to = reply_id.map(|event_id| Reply {
		in_reply_to: InReplyTo { event_id: event_id.to_owned() },
	});

	content
}
