use std::sync::Arc;

use figment::Figment;
use tracing::subscriber::NoSubscriber;

use super::Server;
use crate::{
	config::{Config, Sources},
	log::{LogLevelReloadHandles, Logging, capture::State},
	metrics::Metrics,
};

fn server() -> Server {
	let raw = Figment::new().merge(("server_name", "test.example"));
	let config = Config::new(&raw).expect("minimal config");

	let log = Logging {
		subscriber: Arc::new(NoSubscriber::new()),
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(State::new()),
	};

	Server::new(config, Sources::default(), None, log, Metrics::new(None))
}

#[test]
fn a_restore_is_claimed_once() {
	let server = server();

	assert!(server.claim_backup_restore());
	assert!(!server.claim_backup_restore());
	assert!(!server.claim_backup_restore());
}

#[test]
fn an_idle_server_reports_no_phase() {
	let server = server();

	assert!(server.progress.report().is_none(), "an idle server reports no phase");

	server.progress.begin("migrate");

	assert!(server.progress.report().is_some(), "a phase in flight reports");

	server.progress.end();

	assert!(server.progress.report().is_none(), "an ended phase reports nothing");
}

#[test]
fn a_phase_change_resets_the_count() {
	let server = server();

	server.progress.begin("first");
	server.progress.advance();
	server.progress.advance();

	let counted = server
		.progress
		.report()
		.expect("a phase in flight");

	assert!(counted.contains("first"), "the report names the step: {counted}");
	assert!(counted.contains("2 done"), "the report counts the items: {counted}");

	server.progress.begin("second");

	let reset = server
		.progress
		.report()
		.expect("a phase in flight");

	assert!(reset.contains("second"), "the report names the new step: {reset}");
	assert!(!reset.contains("done"), "a new step starts from no count: {reset}");
}

#[test]
fn an_expected_total_reports_a_position_against_it() {
	let server = server();

	server.progress.begin("migrate");
	server.progress.expect_total(10);
	server.progress.advance();

	let report = server
		.progress
		.report()
		.expect("a phase in flight");

	assert!(report.contains("1 of 10"), "the report counts against the total: {report}");

	server.progress.enter("pass");

	let entered = server
		.progress
		.report()
		.expect("a phase in flight");

	assert!(entered.contains("migrate / pass"), "the report names the pass: {entered}");
	assert!(!entered.contains(" of "), "a pass starts from no total: {entered}");
}
