#![allow(clippy::wildcard_imports)]
#![allow(clippy::enum_glob_use)]

use std::sync::Arc;

use async_trait::async_trait;
use clap::{Command, CommandFactory, Parser};
use ruma::UserId;
use tuwunel_core::{Err, Result};
use tuwunel_service::{
	Services,
	command::{CommandSystem, CompletionTree},
};

use crate::user::UserCommand;

pub(crate) mod user;

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}
tuwunel_core::rustc_flags_capture! {}

struct UserCommandSystem {
	services: Arc<Services>,
}

#[async_trait]
impl CommandSystem for UserCommandSystem {
	fn parse(&self, command_line: &str) -> Vec<String> {
		command_line
			.split_whitespace()
			.map(ToOwned::to_owned)
			.collect()
	}

	fn get_completion_tree(&self) -> CompletionTree {
		build_completion_tree(&UserCommand::command())
	}

	async fn process(
		&self,
		command_line: &[&str],
		input: &str,
		sender: Option<&UserId>,
	) -> Result<String> {
		let command = match UserCommand::try_parse_from(command_line) {
			| Ok(command) => command,
			| Err(error) => {
				return Ok(error.to_string());
			},
		};

		let Some(sender) = sender else {
			return Err!("Sender required for user commands");
		};

		user::process(command, &Context {
			services: self.services.as_ref(),
			input,
			sender,
		})
		.await
	}
}

pub(crate) struct Context<'a> {
	pub services: &'a Services,
	#[allow(dead_code)]
	pub input: &'a str,
	pub sender: &'a UserId,
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
	let command_system = Arc::new(UserCommandSystem { services: services.clone() });
	services
		.userroom
		.set_user_command_system(command_system);
}
