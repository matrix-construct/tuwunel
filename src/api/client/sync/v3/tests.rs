use std::collections::BTreeMap;

use ruma::{
	EventId, OwnedEventId, UInt,
	api::client::sync::sync_events::v3::{State as RoomState, StateEvents},
	uint,
};

use super::{
	StateAfter,
	joined::notifications::{
		GateInputs, Gates as NotificationGates,
		assemble_threads as assemble_thread_notifications,
		assemble_unread as assemble_unread_notifications,
		compute_gates as compute_notification_gates, in_window, send_thread_counts,
	},
};

const SINCE: u64 = 100;
const NEXT_BATCH: u64 = 200;
const INITIAL_SINCE: u64 = 0;

#[test]
fn state_after_wraps_into_named_variant() {
	let events = StateEvents::default;

	assert!(matches!(StateAfter::Off.wrap(events()), RoomState::Before(_)));
	assert!(matches!(StateAfter::Stable.wrap(events()), RoomState::After(_)));
	assert!(matches!(StateAfter::Unstable.wrap(events()), RoomState::AfterUnstable(_)));

	assert!(!StateAfter::Off.requested());
	assert!(StateAfter::Stable.requested());
	assert!(StateAfter::Unstable.requested());
}

#[test]
fn state_after_selects_unstable_when_both_opted_in() {
	// (use_state_after, use_state_after_unstable)
	assert!(matches!(StateAfter::from((false, false)), StateAfter::Off));
	assert!(matches!(StateAfter::from((true, false)), StateAfter::Stable));
	assert!(matches!(StateAfter::from((false, true)), StateAfter::Unstable));
	assert!(matches!(StateAfter::from((true, true)), StateAfter::Unstable));
}

#[test]
fn busy_round_delivers_reset_zeros() {
	let gates = notification_gates(Some(150), &no_threads(), false, SINCE);

	assert!(gates.send_notification_counts);
	assert!((gates.send_notification_count_filter)(&uint!(0)));
}

#[test]
fn busy_round_without_reset_suppresses_zeros() {
	// A cursor at 50 advanced before this window, so it is not a reset.
	let gates = notification_gates(Some(50), &no_threads(), false, SINCE);

	assert!(gates.send_notification_counts);
	assert!(!(gates.send_notification_count_filter)(&uint!(0)));
	assert!((gates.send_notification_count_filter)(&uint!(5)));
}

#[test]
fn busy_round_without_cursor_sends_counts() {
	let gates = notification_gates(None, &no_threads(), false, SINCE);

	assert!(gates.send_notification_counts);
	assert!(!(gates.send_notification_count_filter)(&uint!(0)));
}

#[test]
fn quiet_round_with_advanced_cursor_sends_zeros() {
	let gates = notification_gates(Some(150), &no_threads(), true, SINCE);

	assert!(gates.send_notification_counts);
	assert!((gates.send_notification_count_filter)(&uint!(0)));
}

#[test]
fn quiet_round_with_stale_cursor_sends_nothing() {
	// A cursor at 50 advanced before this window, so nothing changed.
	let gates = notification_gates(Some(50), &no_threads(), true, SINCE);

	assert!(!gates.send_notification_counts);
	assert!(!(gates.send_notification_count_filter)(&uint!(0)));
}

#[test]
fn quiet_round_without_cursor_suppresses_zeros() {
	let gates = notification_gates(None, &no_threads(), true, SINCE);

	assert!(gates.send_notification_counts);
	assert!(!(gates.send_notification_count_filter)(&uint!(0)));
}

#[test]
fn initial_sync_delivers_reset_zeros() {
	// A client can start a fresh token while still caching a count, so an
	// initial round must be able to reconcile it.
	let gates = notification_gates(Some(150), &no_threads(), false, INITIAL_SINCE);

	assert!(gates.send_notification_counts);
	assert!((gates.send_notification_count_filter)(&uint!(0)));
}

#[test]
fn cursor_past_the_cutoff_delivers_reset_zeros() {
	// The response cutoff is captured before the cursor is read, so a
	// cursor beyond it repeats the zero until the token catches up.
	let gates = notification_gates(Some(NEXT_BATCH + 1), &no_threads(), false, SINCE);

	assert!(gates.send_notification_counts);
	assert!((gates.send_notification_count_filter)(&uint!(0)));
}

#[test]
fn thread_only_reset_delivers_zeros() {
	let root = EventId::parse("$thread:example.org").unwrap();
	let thread_reads = BTreeMap::from([(root, 150)]);

	let gates = notification_gates(None, &thread_reads, false, SINCE);

	assert!(gates.send_notification_counts);
	assert!((gates.send_notification_count_filter)(&uint!(0)));
}

#[test]
fn quiet_round_keeps_unchanged_thread_counts() {
	let unread = EventId::parse("$unread:example.org").unwrap();
	let reset = EventId::parse("$reset:example.org").unwrap();
	let idle = EventId::parse("$idle:example.org").unwrap();
	let thread_counts =
		BTreeMap::from([(unread.clone(), (1, 1)), (reset.clone(), (0, 0)), (idle, (0, 0))]);

	// Only the reset thread's cursor advanced this round.
	let threads = assemble_thread_notifications(Some(thread_counts), |root| *root == reset);

	assert_eq!(threads.keys().collect::<Vec<_>>(), [&reset, &unread]);
	assert_eq!(threads[&unread].notification_count, Some(uint!(1)));
	assert_eq!(threads[&unread].highlight_count, Some(uint!(1)));
	assert_eq!(threads[&reset].notification_count, Some(uint!(0)));
}

#[test]
fn thread_counts_never_report_an_unchanged_room() {
	let unread = EventId::parse("$unread:example.org").unwrap();
	let roots = [&unread];
	let none_admitted = |_: &OwnedEventId| false;
	let admitted = |root: &OwnedEventId| *root == unread;

	let (reported, unreported) = (true, false);

	assert!(!send_thread_counts(unreported, roots, none_admitted));
	assert!(!send_thread_counts(unreported, [], admitted));
	assert!(send_thread_counts(unreported, roots, admitted));
	assert!(send_thread_counts(reported, roots, none_admitted));
	assert!(send_thread_counts(reported, [], none_admitted));
}

#[test]
fn thread_totals_merge_only_without_opt_in() {
	let root = EventId::parse("$thread:example.org").unwrap();
	let thread_counts = BTreeMap::from([(root, (2, 1))]);
	let unread = |want_thread_unread| {
		assemble_unread_notifications(
			Some(uint!(3)),
			Some(uint!(0)),
			Some(&thread_counts),
			|_: &UInt| true,
			want_thread_unread,
		)
	};

	let merged = unread(false);
	let partitioned = unread(true);

	assert_eq!(merged.notification_count, Some(uint!(5)));
	assert_eq!(merged.highlight_count, Some(uint!(1)));
	assert_eq!(partitioned.notification_count, Some(uint!(3)));
	assert_eq!(partitioned.highlight_count, Some(uint!(0)));
}

fn notification_gates(
	last_read: Option<u64>,
	thread_reads: &BTreeMap<OwnedEventId, u64>,
	quiet_round: bool,
	since: u64,
) -> NotificationGates<impl Fn(&UInt) -> bool + use<>> {
	compute_notification_gates(
		GateInputs {
			last_notification_read: last_read,
			thread_last_reads: thread_reads,
			quiet_round,
			since,
		},
		in_window(since, NEXT_BATCH),
	)
}

fn no_threads() -> BTreeMap<OwnedEventId, u64> { BTreeMap::new() }
