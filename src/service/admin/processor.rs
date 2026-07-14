use std::{
	fmt::Write,
	mem::take,
	panic::AssertUnwindSafe,
	sync::{Arc, Mutex},
	time::SystemTime,
};

use futures::future::FutureExt;
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

use super::{Command, CommandInput, CommandOutput, Context, ProcessorResult};
use crate::Services;

#[tracing::instrument(level = "debug", skip_all, name = "admin")]
pub(super) async fn handle_command(
	command: Arc<dyn Command>,
	services: Arc<Services>,
	input: &CommandInput,
) -> ProcessorResult {
	AssertUnwindSafe(Box::pin(process_command(&*command, services, input)))
		.catch_unwind()
		.await
		.map_err(Error::from_panic)
		.unwrap_or_else(|error| handle_panic(&error))
}

#[must_use]
pub(super) fn complete(mut cmd: clap::Command, line: &str) -> String {
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

async fn process_command(
	command: &dyn Command,
	services: Arc<Services>,
	input: &CommandInput,
) -> ProcessorResult {
	let (matches, args, body) = parse(&services, command.clap(), input)?;

	let context = Context {
		services: &services,
		body: &body,
		timer: SystemTime::now(),
		output: String::new().into(),
	};

	let (result, mut logs) = process(&context, command, matches, &args).await;

	let output = take(&mut *context.output.lock().await);

	match result {
		| Ok(()) if logs.is_empty() => Ok(Some(CommandOutput::Markdown(output))),

		| Ok(()) => {
			logs.write_str(output.as_str())
				.expect("output buffer");

			Ok(Some(CommandOutput::Markdown(logs)))
		},
		| Err(error) => {
			write!(&mut logs, "Command failed with error:\n```\n{error:#?}\n```")
				.expect("output buffer");

			Err(CommandOutput::Markdown(logs))
		},
	}
}

fn handle_panic(error: &Error) -> ProcessorResult {
	let link =
		"Please submit a [bug report](https://github.com/matrix-construct/tuwunel/issues/new). \
		 🥺";

	let msg = format!("Panic occurred while processing command:\n```\n{error:#?}\n```\n{link}");

	error!("Panic while processing command: {error:?}");
	Err(CommandOutput::Markdown(msg))
}

async fn process(
	context: &Context<'_>,
	command: &dyn Command,
	matches: clap::ArgMatches,
	args: &[String],
) -> (Result, String) {
	let (capture, logs) = capture_create(context);

	let capture_scope = capture.start();
	let result = Box::pin(command.dispatch(matches, context)).await;
	drop(capture_scope);

	debug!(
		ok = result.is_ok(),
		elapsed = ?context.timer.elapsed(),
		command = ?args,
		"command processed"
	);

	let mut output = String::new();

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

fn parse<'a>(
	services: &Arc<Services>,
	cmd: clap::Command,
	input: &'a CommandInput,
) -> Result<(clap::ArgMatches, Vec<String>, Vec<&'a str>), CommandOutput> {
	let lines = input
		.command
		.lines()
		.filter(|line| !line.trim().is_empty());

	let command_line = lines
		.clone()
		.next()
		.expect("command missing first line");

	let body = lines.skip(1).collect();

	match parse_command(cmd, command_line) {
		| Ok((matches, args)) => Ok((matches, args, body)),
		| Err(error) => {
			let message = error
				.to_string()
				.replace("server.name", services.globals.server_name().as_str());

			Err(CommandOutput::Plain(message))
		},
	}
}

fn parse_command(
	mut cmd: clap::Command,
	line: &str,
) -> Result<(clap::ArgMatches, Vec<String>), clap::Error> {
	let argv = parse_line(line);
	let matches = cmd.try_get_matches_from_mut(&argv)?;

	Ok((matches, argv))
}

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
