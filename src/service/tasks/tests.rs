use std::collections::BTreeMap;

use super::{CAPACITY, RETENTION_MS, Status, Task, TaskId, prune_tasks};

fn key(s: &str) -> TaskId { TaskId::from(s).expect("id fits") }

fn task(status: Status, timestamp_ms: u64) -> Task {
	Task {
		action: "test",
		resource_id: String::new(),
		status,
		timestamp_ms,
		result: None,
		error: None,
		handle: None,
	}
}

#[test]
fn terminal_classification() {
	assert!(!Status::Scheduled.is_terminal());
	assert!(!Status::Active.is_terminal());
	assert!(Status::Complete.is_terminal());
	assert!(Status::Failed.is_terminal());
}

#[test]
fn prune_keeps_active_and_recent() {
	let now = RETENTION_MS.saturating_mul(10);
	let mut tasks = BTreeMap::from([
		(key("stale_active"), task(Status::Active, 0)),
		(key("stale_done"), task(Status::Complete, 0)),
		(key("fresh_done"), task(Status::Complete, now)),
	]);

	prune_tasks(&mut tasks, now);

	assert!(tasks.contains_key("stale_active"), "non-terminal survives any age");
	assert!(!tasks.contains_key("stale_done"), "terminal past retention is pruned");
	assert!(tasks.contains_key("fresh_done"), "recent terminal is kept");
}

#[test]
fn prune_caps_terminal_tasks() {
	let now: u64 = 100_000;
	let overflow = CAPACITY.saturating_add(50);
	let mut tasks = BTreeMap::new();
	for i in 0..overflow {
		let ts = now.saturating_sub(u64::try_from(i).expect("fits"));
		tasks.insert(key(&format!("t{i}")), task(Status::Complete, ts));
	}

	prune_tasks(&mut tasks, now);

	assert!(tasks.len() <= CAPACITY, "terminal tasks capped");
	assert!(tasks.contains_key("t0"), "the newest survivor is kept");
}
