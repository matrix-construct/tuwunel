//! Checks how backfill picks each request after the first.
//!
//! The first move off the oldest event is not counted, also after a prepend,
//! and a frontier seed is asked of every candidate. Every branch of a withheld
//! fork is asked before the call ends while the budget allows, and a frontier
//! seed that cannot progress hands the call back to the oldest event's
//! candidates first.

use ruma::{EventId, OwnedEventId, ServerName, event_id, server_name};

use super::{Attempt, Batch, Candidates, Seed, retry, walks};

#[test]
fn the_first_move_off_the_oldest_event_is_not_counted() {
	let (origin, other) = (server_name!("origin.example"), server_name!("other.example"));
	let (oldest, seed) = (event_id!("$oldest"), event_id!("$withheld"));
	let before = attempt(Seed::Oldest, &[origin, other], &[], 3);

	assert_eq!(
		retry(before, &empty(origin), oldest, &frontier(&[oldest, seed])),
		Some(attempt(branch(seed), &[other], &[seed], 3))
	);
}

#[test]
fn an_unmoved_oldest_seed_drops_the_origin_and_counts() {
	let (origin, other) = (server_name!("origin.example"), server_name!("other.example"));
	let oldest = event_id!("$oldest");
	let before = || attempt(Seed::Oldest, &[origin, other], &[], 3);
	let walked = frontier(&[oldest]);

	assert_eq!(
		retry(before(), &empty(origin), oldest, &walked),
		Some(attempt(Seed::Oldest, &[other], &[], 2))
	);

	assert_eq!(retry(before(), &Batch::Unanswered, oldest, &walked), None);
	assert_eq!(retry(before(), &Batch::Prepended, oldest, &walked), None);

	let alone = attempt(Seed::Oldest, &[origin], &[], 3);

	assert_eq!(retry(alone, &empty(origin), oldest, &walked), None);

	let last = attempt(Seed::Oldest, &[origin, other], &[], 1);

	assert_eq!(retry(last, &empty(origin), oldest, &walked), None);
}

#[test]
fn a_prepend_from_the_oldest_event_still_asks_a_withheld_branch() {
	let (origin, other) = (server_name!("origin.example"), server_name!("other.example"));
	let (oldest, right) = (event_id!("$oldest"), event_id!("$right"));
	let before = || attempt(Seed::Oldest, &[origin, other], &[], 3);

	assert_eq!(
		retry(before(), &Batch::Prepended, oldest, &frontier(&[right])),
		Some(attempt(branch(right), &[], &[right], 3))
	);

	assert_eq!(retry(before(), &Batch::Prepended, oldest, &frontier(&[oldest])), None);

	let done = attempt(branch(right), &[], &[right], 3);

	assert_eq!(retry(done, &Batch::Prepended, oldest, &frontier(&[oldest, right])), None);
}

#[test]
fn a_prepend_leaves_no_branch_of_a_withheld_fork_unasked() {
	let other = server_name!("other.example");
	let (oldest, left, right) = (event_id!("$oldest"), event_id!("$left"), event_id!("$right"));
	let before = attempt(branch(left), &[other], &[left], 3);
	let fork = frontier(&[left, right]);
	let after = attempt(branch(right), &[other], &[left, right], 2);

	assert_eq!(retry(before, &Batch::Prepended, oldest, &fork), Some(after));

	let done = || attempt(branch(right), &[other], &[left, right], 2);

	assert_eq!(retry(done(), &Batch::Prepended, oldest, &fork), None);
	assert_eq!(
		retry(done(), &Batch::Prepended, oldest, &frontier(&[oldest, left, right])),
		Some(attempt(Seed::Fallback, &[other], &[left, right], 1))
	);
}

#[test]
fn a_withheld_seed_that_progressed_moves_on_and_counts() {
	let (origin, other) = (server_name!("origin.example"), server_name!("other.example"));
	let (oldest, seed, deeper) =
		(event_id!("$oldest"), event_id!("$withheld"), event_id!("$deeper"));

	let before = || attempt(branch(seed), &[other], &[seed], 3);
	let moved = frontier(&[deeper]);

	assert_eq!(
		retry(before(), &empty(origin), oldest, &moved),
		Some(attempt(branch(deeper), &[other], &[seed, deeper], 2))
	);

	let last = attempt(branch(seed), &[other], &[seed], 1);

	assert_eq!(retry(last, &empty(origin), oldest, &moved), None);
}

#[test]
fn a_withheld_seed_without_progress_returns_to_the_oldest_event() {
	let (origin, other) = (server_name!("origin.example"), server_name!("other.example"));
	let (oldest, seed, sibling) =
		(event_id!("$oldest"), event_id!("$withheld"), event_id!("$sibling"));

	let before = || attempt(branch(seed), &[other], &[seed], 3);
	let fork = frontier(&[seed, sibling]);
	let fallback = Some(attempt(Seed::Fallback, &[other], &[seed], 2));

	assert_eq!(retry(before(), &Batch::Unanswered, oldest, &fork), fallback);
	assert_eq!(retry(before(), &empty(origin), oldest, &fork), fallback);

	let unanchored = || attempt(branch(seed), &[], &[seed], 3);

	assert_eq!(
		retry(unanchored(), &Batch::Unanswered, oldest, &fork),
		Some(attempt(branch(sibling), &[], &[seed, sibling], 2))
	);

	assert_eq!(retry(unanchored(), &empty(origin), oldest, &frontier(&[seed])), None);
}

#[test]
fn the_oldest_events_candidates_come_before_a_later_branch() {
	let (origin, other) = (server_name!("origin.example"), server_name!("other.example"));
	let (oldest, seed, sibling) =
		(event_id!("$oldest"), event_id!("$withheld"), event_id!("$sibling"));

	let fork = frontier(&[oldest, seed, sibling]);
	let fallback = |anchor: &[&ServerName]| attempt(Seed::Fallback, anchor, &[seed], 3);
	let next = Some(attempt(branch(sibling), &[], &[seed, sibling], 2));

	assert_eq!(
		retry(fallback(&[origin, other]), &empty(origin), oldest, &fork),
		Some(attempt(Seed::Fallback, &[other], &[seed], 2))
	);

	assert_eq!(retry(fallback(&[origin]), &empty(origin), oldest, &fork), next);
	assert_eq!(retry(fallback(&[origin, other]), &Batch::Prepended, oldest, &fork), next);
	assert_eq!(retry(fallback(&[origin, other]), &Batch::Unanswered, oldest, &fork), next);

	let spent = frontier(&[oldest, seed]);

	assert_eq!(retry(fallback(&[origin, other]), &Batch::Prepended, oldest, &spent), None);
	assert_eq!(retry(fallback(&[origin]), &empty(origin), oldest, &spent), None);
}

#[test]
fn the_oldest_event_keeps_its_retries_when_the_walk_starts_above_it() {
	let (origin, other, third) = (
		server_name!("origin.example"),
		server_name!("other.example"),
		server_name!("third.example"),
	);

	let (oldest, edge) = (event_id!("$oldest"), event_id!("$edge"));
	let walked = frontier(&[edge]);
	let first = attempt(Seed::Oldest, &[origin, other, third], &[], 3);
	let moved = || attempt(branch(edge), &[other, third], &[edge], 3);
	let fallback =
		|anchor: &[&ServerName], remaining| attempt(Seed::Fallback, anchor, &[edge], remaining);

	assert_eq!(retry(first, &empty(origin), oldest, &walked), Some(moved()));

	let unmoved = retry(moved(), &empty(origin), oldest, &walked);

	assert_eq!(unmoved, Some(fallback(&[other, third], 2)));

	let retried = retry(fallback(&[other, third], 2), &empty(other), oldest, &walked);

	assert_eq!(retried, Some(fallback(&[third], 1)));
}

#[test]
fn the_walk_runs_only_while_a_request_can_follow() {
	let origin = server_name!("origin.example");
	let seed = event_id!("$withheld");

	assert!(walks(&attempt(Seed::Oldest, &[origin], &[], 1), &empty(origin)));
	assert!(walks(&attempt(Seed::Oldest, &[origin], &[], 3), &Batch::Prepended));
	assert!(!walks(&attempt(Seed::Oldest, &[origin], &[], 3), &Batch::Unanswered));
	assert!(walks(&attempt(branch(seed), &[], &[seed], 2), &Batch::Prepended));
	assert!(!walks(&attempt(branch(seed), &[], &[seed], 1), &Batch::Unanswered));
	assert!(!walks(&attempt(Seed::Fallback, &[origin], &[seed], 1), &empty(origin)));
}

fn attempt(seed: Seed, anchor: &[&ServerName], asked: &[&EventId], remaining: usize) -> Attempt {
	let asked = asked
		.iter()
		.copied()
		.map(ToOwned::to_owned)
		.collect();

	Attempt {
		seed,
		anchor: candidates(anchor),
		asked,
		remaining,
	}
}

fn branch(seed: &EventId) -> Seed { Seed::Frontier(seed.to_owned()) }

fn empty(origin: &ServerName) -> Batch { Batch::Empty(origin.to_owned()) }

fn frontier(event_ids: &[&EventId]) -> Vec<OwnedEventId> {
	event_ids
		.iter()
		.copied()
		.map(ToOwned::to_owned)
		.collect()
}

fn candidates(names: &[&ServerName]) -> Candidates {
	names
		.iter()
		.copied()
		.map(ToOwned::to_owned)
		.collect()
}
