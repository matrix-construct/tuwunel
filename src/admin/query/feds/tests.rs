use clap::Parser;
use tuwunel_service::federation::Classification;

use super::*;
use crate::{admin::AdminCommand, query::QueryCommand};

#[test]
fn command_width_defaults_match_request_costs() {
	assert_eq!(version::WIDTH_DEFAULT.get(), 192);
	assert_eq!(event::WIDTH_DEFAULT.get(), 192);
	assert_eq!(head::WIDTH_DEFAULT.get(), 192);
	assert_eq!(ping::WIDTH_DEFAULT.get(), 192);
	assert_eq!(state::WIDTH_DEFAULT.get(), 16);
}

#[test]
fn listing_modes_are_mutually_exclusive() {
	for command in ["version", "ping"] {
		for option in ["--list", "--list-all", "--list-errors"] {
			parse_room_command(command, &[option])
				.expect("each listing mode should parse independently");
		}

		parse_room_command(command, &["--list", "--list-errors"])
			.expect_err("listing modes should be mutually exclusive");
	}
}

#[test]
fn backoff_fault_expires_with_the_retry_delay() {
	let backoff = PeerBackoff {
		class: Classification::Transient,
		anchor_secs: 900,
		oldest_secs: 600,
		delay_secs: 60,
	};

	assert!(backoff_fault(&backoff, 960).is_none());
	assert!(retry_after(&backoff, 960).is_none());

	let Some(Fault::Backoff { class, age, retry }) = backoff_fault(&backoff, 930) else {
		panic!("an unexpired backoff should produce a backoff fault");
	};

	assert_eq!(class, Classification::Transient);
	assert_eq!(age, Duration::from_secs(330));
	assert_eq!(retry, Duration::from_secs(30));
}

#[test]
fn version_fields_are_repeatable_and_value_checked() {
	AdminCommand::try_parse_from([
		"admin",
		"query",
		"feds",
		"version",
		"!room:example.org",
		"--field=name",
		"--field=version",
		"--field=compiler",
	])
	.expect("each version field should parse in one command");

	for field in ["commit", "kernel", "arch", "operating-system"] {
		let option = format!("--field={field}");

		AdminCommand::try_parse_from([
			"admin",
			"query",
			"feds",
			"version",
			"!room:example.org",
			option.as_str(),
		])
		.expect_err("unknown version fields should be rejected");
	}
}

#[test]
fn sort_requires_a_listing_mode() {
	for command in ["version", "ping"] {
		for column in ["origin", "elapsed", "fault"] {
			parse_room_command(command, &["--list-all", "--sort", column])
				.expect("each sort column should parse with a listing mode");
		}

		parse_room_command(command, &["--sort", "elapsed"])
			.expect_err("sorting should require a listing mode");
	}
}

#[test]
fn event_verification_defaults_on_and_switches_independently() {
	assert_eq!(
		event_verification(["admin", "query", "feds", "event", "$event:example.org"]),
		(true, true),
	);

	assert_eq!(
		event_verification([
			"admin",
			"query",
			"feds",
			"event",
			"$event:example.org",
			"--verify-hash",
			"false",
		]),
		(false, true),
	);

	assert_eq!(
		event_verification([
			"admin",
			"query",
			"feds",
			"event",
			"$event:example.org",
			"--verify-signature",
			"false",
		]),
		(true, false),
	);
}

#[test]
fn event_room_and_sweep_options_parse_after_event_id() {
	let command = AdminCommand::try_parse_from([
		"admin",
		"query",
		"feds",
		"event",
		"$event:example.org",
		"!room:example.org",
		"--width",
		"3",
		"--timeout",
		"4",
		"--budget",
		"5",
		"--no-loopback",
	])
	.expect("event command should accept a room and sweep options");

	let AdminCommand::Query(QueryCommand::Feds(FedsCommand::Event {
		room: Some(room),
		sweep,
		..
	})) = command
	else {
		panic!("event command should select the event variant with a room");
	};

	assert_eq!(room.as_str(), "!room:example.org");
	assert_eq!(
		sweep
			.width
			.expect("explicit width should be present")
			.get(),
		3
	);

	assert_eq!(sweep.timeout, 4);
	assert_eq!(sweep.budget, 5);
	assert!(sweep.no_loopback);
}

fn parse_room_command(command: &str, args: &[&str]) -> clap::error::Result<AdminCommand> {
	let prefix = ["admin", "query", "feds", command, "!room:example.org"];

	AdminCommand::try_parse_from(prefix.iter().chain(args))
}

fn event_verification<const N: usize>(args: [&str; N]) -> (bool, bool) {
	let command = AdminCommand::try_parse_from(args).expect("event command should parse");

	let AdminCommand::Query(QueryCommand::Feds(FedsCommand::Event {
		verify_hash,
		verify_signature,
		..
	})) = command
	else {
		panic!("event command should select the event variant");
	};

	(verify_hash, verify_signature)
}
