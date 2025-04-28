#![cfg(test)]

#[test]
fn get_help_short() {
	get_help_inner("-h");
}

#[test]
fn get_help_long() {
	get_help_inner("--help");
}

#[test]
fn get_help_subcommand() {
	get_help_inner("help");
}

fn get_help_inner(input: &str) {
	use clap::Parser;

	use crate::admin::AdminCommand;

	let Err(error) = AdminCommand::try_parse_from(["argv[0] doesn't matter", input]) else {
		panic!("no error!");
	};

	let error = error.to_string();
	// Search for a handful of keywords that suggest the help printed properly
	assert!(error.contains("Usage:"));
	assert!(error.contains("Commands:"));
	assert!(error.contains("Options:"));
}
