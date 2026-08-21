use clap::Parser;

use super::*;
use crate::{admin::AdminCommand, query::QueryCommand};

#[test]
fn command_width_defaults_match_request_costs() {
	assert_eq!(version::WIDTH_DEFAULT.get(), 192);
	assert_eq!(event::WIDTH_DEFAULT.get(), 192);
	assert_eq!(head::WIDTH_DEFAULT.get(), 192);
	assert_eq!(state::WIDTH_DEFAULT.get(), 16);
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
