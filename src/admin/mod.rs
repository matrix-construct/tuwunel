#![allow(clippy::wildcard_imports)]
#![allow(clippy::enum_glob_use)]
#![allow(clippy::large_futures)]

pub(crate) mod admin;
mod tests;
pub(crate) mod utils;

pub(crate) mod appservices;
pub(crate) mod check;
pub(crate) mod debug;
pub(crate) mod federation;
pub(crate) mod media;
pub(crate) mod query;
pub(crate) mod rooms;
pub(crate) mod server;
pub(crate) mod users;

use std::sync::Arc;

use async_trait::async_trait;
use clap::{Command, CommandFactory, Parser};
use ruma::UserId;
use tuwunel_core::{Result, err, trace};
pub(crate) use tuwunel_macros::{command, command_dispatch};
use tuwunel_service::{
	Services,
	command::{CommandSystem, CompletionTree},
};

use crate::admin::AdminCommand;
pub(crate) use crate::utils::get_room_info;

pub(crate) const PAGE_SIZE: usize = 100;

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}
tuwunel_core::rustc_flags_capture! {}

struct AdminCommandSystem {
	services: Arc<Services>,
}

#[async_trait]
impl CommandSystem for AdminCommandSystem {
	fn parse(&self, command_line: &str) -> Vec<String> { parse(command_line) }

	fn get_completion_tree(&self) -> CompletionTree {
		build_completion_tree(&AdminCommand::command())
	}

	async fn process(
		&self,
		command_line: &[&str],
		input: &str,
		_sender: Option<&UserId>,
	) -> Result<String> {
		let command = AdminCommand::try_parse_from(command_line)
			.map_err(|e| err!("Failed to parse command:\n{e}"))?;

		admin::process(command, &Context { services: self.services.as_ref(), input }).await
	}
}

pub(crate) struct Context<'a> {
	pub services: &'a Services,
	pub input: &'a str,
}

fn build_completion_tree(command: &Command) -> CompletionTree {
	CompletionTree {
		name: command.get_name().to_owned(),
		nodes: command
			.get_subcommands()
			.map(build_completion_tree)
			.collect::<Vec<CompletionTree>>(),
	}
}

/// Install the admin command processor
pub async fn init(services: &Arc<Services>) {
	let command_system = Arc::new(AdminCommandSystem { services: services.clone() });
	services
		.admin
		.set_admin_command_system(command_system);
}

#[inline]
fn parse(command_line: &str) -> Vec<String> {
	let mut args = command_line
		.split_whitespace()
		.map(str::to_owned)
		.collect::<Vec<String>>();

	// First indice has to be "admin" but for console convenience we add it here
	if !args.is_empty() && !args[0].ends_with("admin") && !args[0].starts_with('@') {
		args.insert(0, "admin".to_owned());
	}

	// Replace `help command` with `command --help`
	// Clap has a help subcommand, but it omits the long help description.
	if args.len() > 1 && args[1] == "help" {
		args.remove(1);
		args.push("--help".to_owned());
	}

	// Backwards compatibility with `register_appservice`-style commands
	if args.len() > 1 && args[1].contains('_') {
		args[1] = args[1].replace('_', "-");
	}

	// Backwards compatibility with `register_appservice`-style commands
	if args.len() > 2 && args[2].contains('_') {
		args[2] = args[2].replace('_', "-");
	}

	// if the user is using the `query` command (argv[1]), replace the database
	// function/table calls with underscores to match the codebase
	if args.len() > 3 && args[1].eq("query") {
		args[3] = args[3].replace('_', "-");
	}

	trace!(?command_line, ?args, "parse");

	args
}
