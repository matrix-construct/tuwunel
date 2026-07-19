use std::collections::BTreeMap;

use futures::{StreamExt, stream::select};
use ruma::{EventId, OwnedEventId, RoomId, UserId, events::receipt::ReceiptThread};
use tuwunel_core::{
	Result, implement, trace,
	utils::stream::{ReadyExt, TryIgnore},
};
use tuwunel_database::{Deserialized, Ignore, Interfix};

/// Per-thread unread counts: `(notification, highlight)` keyed by thread root.
type ThreadCounts = BTreeMap<OwnedEventId, (u64, u64)>;

/// Per-thread last-read counts keyed by thread root. Used by sync v3 to
/// gate emission of `unread_thread_notifications` to threads whose read
/// cursor advanced within the sync window.
type ThreadLastReads = BTreeMap<OwnedEventId, u64>;

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub fn reset_notification_counts(&self, user_id: &UserId, room_id: &RoomId) {
	let count = self.services.globals.next_count();

	let userroom_id = (user_id, room_id);
	self.db
		.userroomid_highlightcount
		.put(userroom_id, 0_u64);
	self.db
		.userroomid_notificationcount
		.put(userroom_id, 0_u64);

	let roomuser_id = (room_id, user_id);
	self.db
		.roomuserid_lastnotificationread
		.put(roomuser_id, *count);

	let removed = self.clear_suppressed_room(user_id, room_id);
	if removed > 0 {
		trace!(?user_id, ?room_id, removed, "Cleared suppressed push events after read");
	}
}

/// Reset counts for a single thread within a room. Per-thread rows live in
/// the same CFs as the main `(user, room)` rows; the trailing event id
/// keeps them disjoint. Stamps a per-thread last-read so sync v3 can gate
/// emission of `unread_thread_notifications` to threads that advanced
/// within the window.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub fn reset_thread_notification_counts(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	thread_root: &EventId,
) {
	let count = self.services.globals.next_count();

	let userroom_thread = (user_id, room_id, thread_root);
	self.db
		.userroomid_highlightcount
		.put(userroom_thread, 0_u64);
	self.db
		.userroomid_notificationcount
		.put(userroom_thread, 0_u64);

	let roomuser_thread = (room_id, user_id, thread_root);
	self.db
		.roomuserid_lastnotificationread
		.put(roomuser_thread, *count);
}

/// Clear every per-thread notification, highlight, and last-read row for
/// this user and room. The `Interfix` prefix forces a trailing separator
/// into the scan key, so the legacy 2-tuple main row (which has no
/// trailing separator) is excluded by construction; only 3-tuple thread
/// rows match. Sweeps run sequentially: parallelizing them via `join`
/// triggers a `for<'a> FnMut(&[u8])` Send-not-general-enough cascade
/// through the route handler. Per-thread last-reads use the inverse
/// `(room, user, ...)` order to mirror the existing sync watch prefix.
#[implement(super::Service)]
pub async fn clear_all_thread_notification_counts(&self, user_id: &UserId, room_id: &RoomId) {
	let userroom_prefix = (user_id, room_id, Interfix);
	let roomuser_prefix = (room_id, user_id, Interfix);

	self.db
		.userroomid_highlightcount
		.keys_prefix_raw(&userroom_prefix)
		.ignore_err()
		.ready_for_each(|key| {
			self.db.userroomid_highlightcount.remove(key);
		})
		.await;

	self.db
		.userroomid_notificationcount
		.keys_prefix_raw(&userroom_prefix)
		.ignore_err()
		.ready_for_each(|key| {
			self.db.userroomid_notificationcount.remove(key);
		})
		.await;

	self.db
		.roomuserid_lastnotificationread
		.keys_prefix_raw(&roomuser_prefix)
		.ignore_err()
		.ready_for_each(|key| {
			self.db
				.roomuserid_lastnotificationread
				.remove(key);
		})
		.await;
}

/// Dispatcher: route a receipt's `ReceiptThread` to the matching reset path.
/// `Unthreaded` clears all room and thread counts; `Main` clears only the
/// main-timeline counts; `Thread(id)` clears just that thread.
#[implement(super::Service)]
pub async fn reset_notification_counts_for_thread(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	thread: &ReceiptThread,
) {
	match thread {
		| ReceiptThread::Main => self.reset_notification_counts(user_id, room_id),
		| ReceiptThread::Thread(root) =>
			self.reset_thread_notification_counts(user_id, room_id, root),
		| _ => {
			self.reset_notification_counts(user_id, room_id);
			self.clear_all_thread_notification_counts(user_id, room_id)
				.await;
		},
	}
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self), ret(level = "trace"))]
pub async fn notification_count(&self, user_id: &UserId, room_id: &RoomId) -> u64 {
	let key = (user_id, room_id);
	self.db
		.userroomid_notificationcount
		.qry(&key)
		.await
		.deserialized()
		.unwrap_or(0)
}

/// Total unread notifications for a user across all room-main and thread rows.
///
/// Push Gateway `counts.unread` is a global value, so event and counts-only
/// pushes must both use this single aggregation path.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self), ret(level = "trace"))]
pub async fn global_notification_count(&self, user_id: &UserId) -> u64 {
	self.db
		.userroomid_notificationcount
		.stream_prefix(&(user_id, Interfix))
		.ignore_err()
		.map(|(_, count): (Ignore, u64)| count)
		.ready_fold(0_u64, u64::saturating_add)
		.await
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self), ret(level = "trace"))]
pub async fn highlight_count(&self, user_id: &UserId, room_id: &RoomId) -> u64 {
	let key = (user_id, room_id);
	self.db
		.userroomid_highlightcount
		.qry(&key)
		.await
		.deserialized()
		.unwrap_or(0)
}

/// Per-thread `(notification, highlight)` counts for one room and user.
/// `Interfix` excludes the legacy 2-tuple main row from the scan; only
/// 3-tuple `(user, room, root)` rows match.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn thread_notification_counts(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> ThreadCounts {
	let prefix = (user_id, room_id, Interfix);
	let notifications = self
		.db
		.userroomid_notificationcount
		.stream_prefix(&prefix)
		.ignore_err()
		.map(notification_kv);

	let highlights = self
		.db
		.userroomid_highlightcount
		.stream_prefix(&prefix)
		.ignore_err()
		.map(highlight_kv);

	select(notifications, highlights)
		.ready_fold(ThreadCounts::default(), merge_thread_count)
		.await
}

fn notification_kv(
	(key, notifications): ((&UserId, &RoomId, OwnedEventId), u64),
) -> (OwnedEventId, (u64, u64)) {
	(key.2, (notifications, 0))
}

fn highlight_kv(
	(key, highlights): ((&UserId, &RoomId, OwnedEventId), u64),
) -> (OwnedEventId, (u64, u64)) {
	(key.2, (0, highlights))
}

fn merge_thread_count(
	mut counts: ThreadCounts,
	(root, (notifications, highlights)): (OwnedEventId, (u64, u64)),
) -> ThreadCounts {
	let entry = counts.entry(root).or_default();
	entry.0 = entry.0.saturating_add(notifications);
	entry.1 = entry.1.saturating_add(highlights);
	counts
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self), ret(level = "trace"))]
pub async fn last_notification_read(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
	let key = (room_id, user_id);
	self.db
		.roomuserid_lastnotificationread
		.qry(&key)
		.await
		.deserialized()
}

/// Per-thread last-read counts for one room and user. `Interfix` keeps the
/// scan to 3-tuple `(room, user, root)` rows; the legacy 2-tuple main row
/// is excluded by construction and lives behind `last_notification_read`.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn thread_last_notification_reads(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> ThreadLastReads {
	let prefix = (room_id, user_id, Interfix);
	self.db
		.roomuserid_lastnotificationread
		.stream_prefix(&prefix)
		.ignore_err()
		.map(|((_, _, root), count): ((Ignore, Ignore, OwnedEventId), u64)| (root, count))
		.collect()
		.await
}

#[implement(super::Service)]
pub async fn delete_room_notification_read(&self, room_id: &RoomId) -> Result {
	let key = (room_id, Interfix);
	self.db
		.roomuserid_lastnotificationread
		.keys_prefix_raw(&key)
		.ignore_err()
		.ready_for_each(|key| {
			trace!("Removing key: {key:?}");
			self.db
				.roomuserid_lastnotificationread
				.remove(key);
		})
		.await;

	Ok(())
}
