use std::collections::BTreeMap;

use futures::FutureExt;
use ruma::{
	OwnedEventId, RoomId, UInt, UserId, api::client::sync::sync_events::UnreadNotificationsCount,
	uint,
};
use tuwunel_core::utils::{BoolExt, TryFutureExtExt};
use tuwunel_service::Services;

use super::ThreadCounts;

pub(in super::super) struct GateInputs<'a> {
	pub(in super::super) last_notification_read: Option<u64>,
	pub(in super::super) thread_last_reads: &'a BTreeMap<OwnedEventId, u64>,
	pub(in super::super) quiet_round: bool,
	pub(in super::super) since: u64,
}

pub(in super::super) struct Gates<F> {
	pub(in super::super) send_notification_counts: bool,
	pub(in super::super) send_notification_count_filter: F,
}

pub(in super::super) fn count_futures<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	room_id: &'a RoomId,
	send: bool,
) -> (
	impl Future<Output = Option<UInt>> + Send + 'a,
	impl Future<Output = Option<UInt>> + Send + 'a,
	impl Future<Output = Option<ThreadCounts>> + Send + 'a,
) {
	let notification_count = send.then_async(move || {
		services
			.pusher
			.notification_count(sender_user, room_id)
			.map(TryInto::try_into)
			.unwrap_or(uint!(0))
	});

	let highlight_count = send.then_async(move || {
		services
			.pusher
			.highlight_count(sender_user, room_id)
			.map(TryInto::try_into)
			.unwrap_or(uint!(0))
	});

	// MSC3773: per-thread counts.
	let thread_counts = send.then_async(move || {
		services
			.pusher
			.thread_notification_counts(sender_user, room_id)
	});

	(notification_count, highlight_count, thread_counts)
}

/// The window a sync round reports a cursor advance in.
///
/// The lower bound is the round's own token and the upper bound is the
/// response cutoff captured before the room is assembled, so a cursor stamped
/// after that cutoff falls outside it.
pub(in super::super) fn in_window(since: u64, next_batch: u64) -> impl Fn(u64) -> bool + Copy {
	move |count| count > since && count <= next_batch
}

/// The gates deciding what notification counts a joined room reports.
///
/// A room count, or a zero thread count, reaches the client when the round
/// carried timeline events or a read cursor advanced within the window. An
/// explicit zero room count additionally needs a cursor newer than the request
/// token, and because the response cutoff is captured before that cursor is
/// read, a cursor past the cutoff repeats the zero until the token catches up.
pub(in super::super) fn compute_gates<F: Fn(u64) -> bool>(
	GateInputs {
		last_notification_read,
		thread_last_reads,
		quiet_round,
		since,
	}: GateInputs<'_>,
	in_window: F,
) -> Gates<impl Fn(&UInt) -> bool + use<F>> {
	// Thread-only resets leave the main cursor alone, so without the thread leg
	// a quiet round would never report them.
	let send_notification_counts = !quiet_round
		|| last_notification_read.is_none_or(&in_window)
		|| thread_last_reads
			.values()
			.copied()
			.any(&in_window);

	let after_since = |count: u64| count > since;

	// A cursor newer than since means the client may still cache the pre-reset
	// count, so allow an explicit zero to reconcile it.
	let send_notification_resets = last_notification_read.is_some_and(after_since)
		|| thread_last_reads
			.values()
			.copied()
			.any(&after_since);

	let send_notification_count_filter =
		move |count: &UInt| *count != uint!(0) || send_notification_resets;

	Gates {
		send_notification_counts,
		send_notification_count_filter,
	}
}

pub(in super::super) fn assemble_unread(
	notification_count: Option<UInt>,
	highlight_count: Option<UInt>,
	thread_counts: Option<&ThreadCounts>,
	send_notification_count_filter: impl Fn(&UInt) -> bool,
	want_thread_unread: bool,
) -> UnreadNotificationsCount {
	// MSC3773: an opted-in client takes thread counts per thread, so they stay
	// out of the room totals.
	let (thread_total_notifications, thread_total_highlights) = thread_counts
		.filter(|_| want_thread_unread.is_false())
		.into_iter()
		.flat_map(BTreeMap::values)
		.fold((0_u64, 0_u64), |(n, h), &(notifs, hl)| {
			(n.saturating_add(notifs), h.saturating_add(hl))
		});

	let merge_total = |total: u64| {
		move |count: UInt| count.saturating_add(UInt::try_from(total).unwrap_or_default())
	};

	UnreadNotificationsCount {
		highlight_count: highlight_count
			.map(merge_total(thread_total_highlights))
			.filter(&send_notification_count_filter),
		notification_count: notification_count
			.map(merge_total(thread_total_notifications))
			.filter(&send_notification_count_filter),
	}
}

/// Whether a joined-room entry carries MSC3773 per-thread counts.
///
/// A thread `admitted` names may report an otherwise empty room. Any other
/// count only rides on a room the round already reports, or a room whose only
/// change is a count would repeat in every round.
pub(in super::super) fn send_thread_counts<'a, I>(
	reported: bool,
	thread_roots: I,
	admitted: impl Fn(&OwnedEventId) -> bool,
) -> bool
where
	I: IntoIterator<Item = &'a OwnedEventId>,
{
	reported || thread_roots.into_iter().any(admitted)
}

/// MSC3773 per-thread counts for a room the round reports.
///
/// Clients read a thread missing from the map as having no notifications
/// (matrix-js-sdk clears that thread's badge), so every non-zero count is sent
/// whether or not it changed. A zero is sent only where `send_zero` admits it.
pub(in super::super) fn assemble_threads(
	thread_counts: Option<ThreadCounts>,
	send_zero: impl Fn(&OwnedEventId) -> bool,
) -> BTreeMap<OwnedEventId, UnreadNotificationsCount> {
	thread_counts
		.into_iter()
		.flatten()
		.filter(|(root, counts)| *counts != (0, 0) || send_zero(root))
		.map(|(root, (notifications, highlights))| {
			let counts = UnreadNotificationsCount {
				notification_count: UInt::try_from(notifications).ok(),
				highlight_count: UInt::try_from(highlights).ok(),
			};

			(root, counts)
		})
		.collect()
}
