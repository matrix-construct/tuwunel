#![cfg(test)]

use clap::Parser;

use crate::{admin::AdminCommand, media::MediaCommand, query::QueryCommand};

#[test]
fn get_help_short() { get_help_inner("-h"); }

#[test]
fn get_help_long() { get_help_inner("--help"); }

#[test]
fn get_help_subcommand() { get_help_inner("help"); }

#[test]
fn delete_backups_requires_keep() {
	assert!(
		parse_err(&["argv[0] doesn't matter", "server", "delete-backups"]).contains("KEEP"),
		"deleting every backup must not be the default"
	);
}

#[test]
fn delete_range_requires_direction() {
	assert!(
		parse_err(&["argv[0] doesn't matter", "media", "delete-range", "7d"])
			.contains("--older-than|--newer-than"),
		"a direction flag must be required"
	);
}

#[test]
fn delete_range_rejects_both_directions() {
	assert!(
		parse_err(&[
			"argv[0] doesn't matter",
			"media",
			"delete-range",
			"7d",
			"--older-than",
			"--newer-than",
		])
		.contains("cannot be used with"),
		"the direction flags must be exclusive"
	);
}

#[test]
fn delete_range_accepts_one_direction() {
	for direction in ["--older-than", "-o"] {
		let AdminCommand::Media(MediaCommand::DeleteRange { older_than, newer_than, .. }) =
			parse_ok(&["argv[0] doesn't matter", "media", "delete-range", "7d", direction])
		else {
			panic!("{direction} must parse as a media delete-range command");
		};

		assert!(older_than, "{direction} must select the older-than direction");
		assert!(!newer_than, "{direction} must leave the newer-than direction unset");
	}
}

#[test]
fn query_feds_parse() {
	for survey in ["version", "state", "head"] {
		let command =
			parse_ok(&["argv[0] doesn't matter", "query", "feds", survey, "!room:example.org"]);

		assert!(
			matches!(command, AdminCommand::Query(QueryCommand::Feds(_))),
			"{survey} must parse as a query feds command"
		);
	}
}

#[test]
fn query_feds_event_parse() {
	let command =
		parse_ok(&["argv[0] doesn't matter", "query", "feds", "event", "$event:example.org"]);

	assert!(matches!(command, AdminCommand::Query(QueryCommand::Feds(_))));
}

#[test]
fn query_feds_require_a_room() {
	for survey in ["version", "state", "head"] {
		assert!(
			parse_err(&["argv[0] doesn't matter", "query", "feds", survey]).contains("ROOM"),
			"{survey} must require a room"
		);
	}
}

#[test]
fn query_feds_reject_zero_width() {
	let error = parse_err(&[
		"argv[0] doesn't matter",
		"query",
		"feds",
		"version",
		"!room:example.org",
		"--width",
		"0",
	]);

	assert!(error.contains("invalid value '0'"), "a survey width must be nonzero");
}

fn get_help_inner(input: &str) {
	let error = parse_err(&["argv[0] doesn't matter", input]);

	// Search for a handful of keywords that suggest the help printed properly
	assert!(error.contains("Usage:"));
	assert!(error.contains("Commands:"));
	assert!(error.contains("Options:"));
}

fn parse_err(argv: &[&str]) -> String {
	let Err(error) = AdminCommand::try_parse_from(argv) else {
		panic!("parsing {argv:?} must fail");
	};

	error.to_string()
}

fn parse_ok(argv: &[&str]) -> AdminCommand {
	AdminCommand::try_parse_from(argv)
		.unwrap_or_else(|error| panic!("parsing {argv:?} must succeed: {error}"))
}
