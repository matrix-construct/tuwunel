//! Takes a redacted event back out of the unread counts it raised.
//!
//! The counters only ever grow on append and drop to zero on a receipt, so a
//! redacted event would otherwise stay counted until the user reads past it.

use std::{fmt::Debug, sync::Arc};

use futures::{StreamExt, TryStreamExt, future::join};
use ruma::{
	CanonicalJsonObject, EventId, RoomId, UserId, api::Direction, events::relation::RelationType,
	push::Action,
};
use serde::Serialize;
use tuwunel_core::{
	debug_warn, implement,
	utils::{
		BoolExt,
		stream::{TryReadyExt, automatic_width},
	},
};
use tuwunel_database::{Deserialized, Map};
use tuwunel_matrix::{
	event::{Event, RelationTypeEqual},
	pdu::{Count, Pdu, PduId, RawPduId},
};

use super::{CountMutex, Notified};
use crate::rooms::short::ShortRoomId;

/// Tells whether an action raises a counter.
///
/// `Action::should_notify` raises the notification counter and
/// `Action::is_highlight` the highlight counter, as on append.
type Raises = fn(&Action) -> bool;

/// One of the user's two counters, with the lock that guards it.
///
/// The notification and highlight counters differ only in these fields.
#[derive(Clone, Copy)]
struct Counter<'a> {
	counts: &'a Arc<Map>,
	mutex: &'a CountMutex,
	raises: Raises,
}

/// Where a redacted event was counted, the same for every recipient.
///
/// The per-member fan-out copies it into each member's task.
#[derive(Clone, Copy)]
struct Redacted<'a> {
	room_id: &'a RoomId,
	shortroomid: ShortRoomId,
	count: u64,
	thread_root: Option<&'a EventId>,
}

/// Takes a redacted event out of each local member's unread counts.
///
/// As Synapse deletes a redacted event's push actions, each member's
/// notification record for the event is removed, and the counts the event
/// raised drop by one while they still count it. Removing the record first
/// makes a second redaction of the event a no-op. Runs before
/// `timeline::redact_pdu` strips the relations the thread lookup reads.
#[implement(super::Service)]
#[tracing::instrument(name = "redact", level = "debug", skip_all)]
pub(crate) async fn redact_pdu(
	&self,
	pdu_id: &RawPduId,
	event_id: &EventId,
	pdu: &CanonicalJsonObject,
) {
	let PduId { shortroomid, count: Count::Normal(count) } = (*pdu_id).into() else {
		return;
	};

	let Ok(pdu) = Pdu::from_object_and_eventid(event_id, pdu.clone())
		.inspect_err(|error| debug_warn!(%event_id, %error, "Redacted event kept its counts"))
	else {
		return;
	};

	let thread_root = self.services.threads.get_thread_id(&pdu).await;
	let redacted = Redacted {
		room_id: pdu.room_id(),
		shortroomid,
		count,
		thread_root: thread_root.as_deref(),
	};

	self.services
		.state_cache
		.local_users_in_room(pdu.room_id())
		.map(ToOwned::to_owned)
		.for_each_concurrent(automatic_width(), async |user| {
			self.redact_pdu_for_user(&user, redacted).await;
		})
		.await;
}

#[implement(super::Service)]
async fn redact_pdu_for_user(&self, user: &UserId, redacted: Redacted<'_>) {
	let key = (user, redacted.count);
	let Ok(notified) = self
		.db
		.useridcount_notification
		.qry(&key)
		.await
		.deserialized::<Notified>()
	else {
		return;
	};

	self.db.useridcount_notification.del(key);

	let notifications = Counter {
		counts: &self.db.userroomid_notificationcount,
		mutex: &self.notification_increment_mutex,
		raises: Action::should_notify,
	};

	let highlights = Counter {
		counts: &self.db.userroomid_highlightcount,
		mutex: &self.highlight_increment_mutex,
		raises: Action::is_highlight,
	};

	let actions = &notified.actions;
	join(
		self.uncount(notifications, actions, user, redacted),
		self.uncount(highlights, actions, user, redacted),
	)
	.await;
}

/// Subtract the event from one of the user's counters if its actions raised
/// that counter and it still counts the event.
///
/// Runs under the counter's increment lock, which the notification reset
/// also holds while it zeroes its counter. The highlight reset takes no lock,
/// so a receipt racing a redaction can lose its zero, as it can racing an
/// increment.
#[implement(super::Service)]
async fn uncount(
	&self,
	counter: Counter<'_>,
	actions: &[Action],
	user: &UserId,
	redacted: Redacted<'_>,
) {
	if actions.iter().any(counter.raises).is_false() {
		return;
	}

	let Redacted { room_id, thread_root, .. } = redacted;
	let _lock = counter
		.mutex
		.lock(&(room_id.to_owned(), user.to_owned()))
		.await;

	match thread_root {
		| Some(root) =>
			self.uncount_at(counter, (user, room_id, root), user, redacted)
				.await,
		| None =>
			self.uncount_at(counter, (user, room_id), user, redacted)
				.await,
	}
}

#[implement(super::Service)]
async fn uncount_at<K>(&self, counter: Counter<'_>, key: K, user: &UserId, redacted: Redacted<'_>)
where
	K: Serialize + Debug + Send + Sync,
{
	let value: u64 = counter
		.counts
		.qry(&key)
		.await
		.deserialized()
		.unwrap_or(0);

	// An absent or zero counter stays unwritten, so a cleared row is not recreated.
	if let Some(lowered) = value.checked_sub(1)
		&& self
			.still_counted(counter, user, redacted, value)
			.await
	{
		counter.counts.put(key, lowered);
	}
}

/// Whether a counter holding `value` still counts the redacted event.
///
/// Every reset zeroes a whole counter, so it holds just its notifying events
/// since the last reset. A thread's own stamp dates that reset, because an
/// unthreaded receipt deletes the thread stamps as it clears their counts. The
/// room stamp dates it for the main timeline, but a main-timeline receipt also
/// moves it without clearing threads, so a thread event behind it is settled
/// by the thread's later replies.
#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn still_counted(
	&self,
	counter: Counter<'_>,
	user: &UserId,
	redacted: Redacted<'_>,
	value: u64,
) -> bool {
	let Redacted { room_id, count, thread_root, .. } = redacted;

	if let Some(root) = thread_root
		&& let Ok(read) = self
			.last_thread_notification_read(user, room_id, root)
			.await
	{
		return count > read;
	}

	let past_room_read = self
		.last_notification_read(user, room_id)
		.await
		.ok()
		.is_none_or(|read| count > read);

	let Some(root) = thread_root.filter(|_| past_room_read.is_false()) else {
		return past_room_read;
	};

	self.counted_by_replies(counter, user, redacted, root, value)
		.await
}

/// Whether fewer than `value` of the thread's later replies raised the counter.
///
/// The counter holds the thread's latest notifying events, so the redacted
/// reply is among them exactly while fewer than `value` later replies raised
/// it for the user. At most `thread_latest_reply_search_limit` relation rows
/// are read; a walk that uses them all, or fails to read one, returns false,
/// as does an unknown or backfilled root, which holds no relations. Only
/// direct thread replies are seen, while the thread bucket takes any event
/// within three relation hops of the root, so a notifying event that joins it
/// by such a hop can leave the count one low.
#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn counted_by_replies(
	&self,
	counter: Counter<'_>,
	user: &UserId,
	Redacted { shortroomid, count, .. }: Redacted<'_>,
	root: &EventId,
	value: u64,
) -> bool {
	let Ok(root @ Count::Normal(_)) = self.services.timeline.get_pdu_count(root).await else {
		return false;
	};

	let limit = usize::try_from(value).unwrap_or(usize::MAX);
	let budget = self
		.services
		.server
		.config
		.rooms
		.redaction
		.thread_latest_reply_search_limit;

	// Err ends the walk: the limit was reached or a row failed to read.
	let walk = self
		.services
		.pdu_metadata
		.relation_counts(root, Some(Count::Normal(count)), Direction::Forward, budget)
		.inspect_err(|error| debug_warn!(%error, "Relation read failed, count kept"))
		.map_err(drop)
		.and_then(async |reply| {
			let raised = self
				.raised_by_reply(counter, user, shortroomid, reply)
				.await;

			Ok(raised)
		})
		.ready_try_fold((0_usize, 0_usize), |(seen, later), raised: bool| {
			let later = later.saturating_add(usize::from(raised));

			later
				.lt(&limit)
				.then_some((seen.saturating_add(1), later))
				.ok_or(())
		})
		.await;

	// A walk the budget ended may have missed the replies that settle it.
	walk.is_ok_and(|(seen, later)| seen < budget && later < limit)
}

/// Whether a reply raised the counter for the user as a direct thread reply.
///
/// The user's notification record decides first, so a reply that never
/// notified them costs no event load.
#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn raised_by_reply(
	&self,
	counter: Counter<'_>,
	user: &UserId,
	shortroomid: ShortRoomId,
	count: Count,
) -> bool {
	let raised = self
		.db
		.useridcount_notification
		.qry(&(user, count.into_unsigned()))
		.await
		.deserialized()
		.is_ok_and(|notified: Notified| notified.actions.iter().any(counter.raises));

	let pdu_id: RawPduId = PduId { shortroomid, count }.into();

	raised
		&& self
			.services
			.timeline
			.get_pdu_from_id(&pdu_id)
			.await
			.is_ok_and(|reply| RelationType::Thread.relation_type_equal(&reply))
}

#[cfg(test)]
mod tests {
	use std::{fmt::Debug, iter::once, sync::Arc};

	use ruma::{
		EventId, RoomId, UserId, event_id,
		push::{Action, HighlightTweakValue, Tweak},
		room_id, user_id,
	};
	use serde::Serialize;
	use serde_json::{Value, json};
	use tuwunel_core::{Result, config::Figment, utils::result::NotFound};
	use tuwunel_database::{Deserialized, Json, Map};
	use tuwunel_matrix::pdu::{Count, PduId, RawPduId};

	use super::Redacted;
	use crate::{
		Services,
		pusher::{Data, Notified},
		test_utils::fixture,
	};

	const SHORTROOMID: u64 = 1;

	/// Redacting an unread thread reply lowers the thread count once.
	///
	/// A second redaction finds no record and changes nothing. A reply behind
	/// the thread's stamp was already cleared by the thread receipt.
	#[tokio::test]
	async fn redaction_uncounts_an_unread_thread_reply_once() -> Result {
		let Some(fixture) = fixture(Figment::new()).await? else {
			return Ok(());
		};

		let pusher = &fixture.services.pusher;
		let Data {
			useridcount_notification: notified,
			userroomid_notificationcount: notifications,
			roomuserid_lastnotificationread: last_reads,
			..
		} = &pusher.db;

		let (user, room_id, root) = ids();
		let thread = (user, room_id, root);

		notifications.put(thread, 2_u64);
		last_reads.put((room_id, user, root), 15_u64);
		notify(notified, user, 10, &[Action::Notify]);
		notify(notified, user, 20, &[Action::Notify]);

		for (count, expected) in [(20, 1), (20, 1), (10, 1)] {
			pusher
				.redact_pdu_for_user(user, redacted(room_id, count, Some(root)))
				.await;

			assert_eq!(stored(notifications, thread).await?, expected, "after {count}");
		}

		for count in [10_u64, 20] {
			assert!(notified.qry(&(user, count)).await.is_err(), "record {count} kept");
		}

		Ok(())
	}

	/// A room event past the room's stamp comes off both room counters.
	///
	/// A thread reply with no thread stamp that is past the room's stamp comes
	/// off the thread count, since no receipt can have cleared it.
	#[tokio::test]
	async fn redaction_uncounts_room_counts_past_the_room_stamp() -> Result {
		let Some(fixture) = fixture(Figment::new()).await? else {
			return Ok(());
		};

		let pusher = &fixture.services.pusher;
		let Data {
			useridcount_notification: notified,
			userroomid_notificationcount: notifications,
			userroomid_highlightcount: highlights,
			roomuserid_lastnotificationread: last_reads,
			..
		} = &pusher.db;

		let (user, room_id, root) = ids();
		let (room, thread) = ((user, room_id), (user, room_id, root));
		let highlight = Action::SetTweak(Tweak::Highlight(HighlightTweakValue::Yes));

		notifications.put(room, 1_u64);
		highlights.put(room, 1_u64);
		notifications.put(thread, 1_u64);
		last_reads.put((room_id, user), 15_u64);
		notify(notified, user, 20, &[Action::Notify, highlight]);
		notify(notified, user, 30, &[Action::Notify]);

		for redaction in [redacted(room_id, 20, None), redacted(room_id, 30, Some(root))] {
			pusher.redact_pdu_for_user(user, redaction).await;
		}

		assert_eq!(stored(notifications, room).await?, 0);
		assert_eq!(stored(highlights, room).await?, 0);
		assert_eq!(stored(notifications, thread).await?, 0);

		Ok(())
	}

	/// A thread reply behind a main-timeline receipt is judged by later replies.
	///
	/// An unthreaded receipt at 25 cleared the thread and deleted its stamp,
	/// and a main-timeline receipt at 40 then moved the room stamp past the
	/// replies. The cleared reply at 20 stays out of the count, which the two
	/// later notifying replies account for, and each of those comes off in
	/// turn. A reply that only highlighted, a notifying reaction to the root,
	/// and a notifying event outside the thread are not counted.
	#[tokio::test]
	async fn redaction_behind_a_main_receipt_counts_later_replies() -> Result {
		let Some(fixture) = fixture(Figment::new()).await? else {
			return Ok(());
		};

		let services = &fixture.services;
		let pusher = &services.pusher;
		let Data {
			useridcount_notification: notified,
			userroomid_notificationcount: notifications,
			roomuserid_lastnotificationread: last_reads,
			..
		} = &pusher.db;

		let (user, room_id, root) = ids();
		let thread = (user, room_id, root);
		let root_count = 10;
		let (notifies, highlights) =
			([Action::Notify], [Action::SetTweak(Tweak::Highlight(HighlightTweakValue::Yes))]);

		let relations = [
			(20, event_id!("$first:localhost"), "m.thread", &notifies),
			(30, event_id!("$second:localhost"), "m.thread", &notifies),
			(35, event_id!("$third:localhost"), "m.thread", &notifies),
			(36, event_id!("$reaction:localhost"), "m.annotation", &notifies),
			(37, event_id!("$mention:localhost"), "m.thread", &highlights),
		];

		let text = json!({ "msgtype": "m.text", "body": "root" });
		store_pdu(services, room_id, root_count, root, &text);

		for (count, event_id, rel_type, actions) in relations {
			store_reply(services, (root, root_count), (event_id, count), rel_type, actions);
		}

		notify(notified, user, 45, &[Action::Notify]);
		notifications.put(thread, 2_u64);
		last_reads.put((room_id, user), 40_u64);

		for (count, expected) in [(20, 2), (30, 1), (35, 0)] {
			pusher
				.redact_pdu_for_user(user, redacted(room_id, count, Some(root)))
				.await;

			assert_eq!(stored(notifications, thread).await?, expected, "after {count}");
		}

		Ok(())
	}

	/// A reply walk cut short by the row budget keeps the count it could not settle.
	///
	/// With a three-row budget, a highlighted reply followed by four or by
	/// exactly three notifying but unhighlighted replies keeps its highlight,
	/// since the rows read reach no verdict. One followed by two is settled
	/// and comes off.
	#[tokio::test]
	async fn redaction_keeps_a_count_the_reply_budget_cannot_settle() -> Result {
		let config =
			Figment::new().merge(("rooms.redaction.thread_latest_reply_search_limit", 3));

		let Some(fixture) = fixture(config).await? else {
			return Ok(());
		};

		let services = &fixture.services;
		let pusher = &services.pusher;
		let Data {
			userroomid_highlightcount: highlights,
			roomuserid_lastnotificationread: last_reads,
			..
		} = &pusher.db;

		let (user, room_id, long_root) = ids();
		let exact_root = event_id!("$exact:localhost");
		let short_root = event_id!("$short:localhost");
		let notifies = [Action::Notify];
		let highlight =
			[Action::Notify, Action::SetTweak(Tweak::Highlight(HighlightTweakValue::Yes))];

		// (root, root count, highlighted reply, later replies, highlights left)
		let text = json!({ "msgtype": "m.text", "body": "root" });
		let threads = [
			(long_root, 10, 20, [30, 31, 32, 33].as_slice(), 1),
			(exact_root, 50, 60, [70, 71, 72].as_slice(), 1),
			(short_root, 80, 90, [100, 101].as_slice(), 0),
		];

		for (root, root_count, highlighted, later, _) in threads {
			store_pdu(services, room_id, root_count, root, &text);
			highlights.put((user, room_id, root), 1_u64);

			let later = later
				.iter()
				.map(|&count| (count, notifies.as_slice()));

			for (count, actions) in once((highlighted, highlight.as_slice())).chain(later) {
				let event_id =
					EventId::parse(format!("$reply{count}:localhost")).expect("reply id");

				store_reply(
					services,
					(root, root_count),
					(&event_id, count),
					"m.thread",
					actions,
				);
			}
		}

		last_reads.put((room_id, user), 200_u64);

		for (root, _, highlighted, _, expected) in threads {
			pusher
				.redact_pdu_for_user(user, redacted(room_id, highlighted, Some(root)))
				.await;

			let count = stored(highlights, (user, room_id, root)).await?;

			assert_eq!(count, expected, "after {highlighted}");
		}

		Ok(())
	}

	fn ids() -> (&'static UserId, &'static RoomId, &'static EventId) {
		(
			user_id!("@alice:localhost"),
			room_id!("!room:localhost"),
			event_id!("$root:localhost"),
		)
	}

	fn notify(notified: &Arc<Map>, user: &UserId, count: u64, actions: &[Action]) {
		let record = Notified {
			ts: 0,
			sroomid: SHORTROOMID,
			tag: None,
			actions: actions.into(),
		};

		notified.put((user, count), Json(record));
	}

	fn redacted<'a>(
		room_id: &'a RoomId,
		count: u64,
		thread_root: Option<&'a EventId>,
	) -> Redacted<'a> {
		Redacted {
			room_id,
			shortroomid: SHORTROOMID,
			count,
			thread_root,
		}
	}

	/// A counter's stored value, zero when absent.
	///
	/// Any other read error fails the test rather than reading as zero.
	async fn stored<K>(counter: &Arc<Map>, key: K) -> Result<u64>
	where
		K: Serialize + Debug,
	{
		counter
			.qry(&key)
			.await
			.deserialized()
			.optional()
			.map(Option::unwrap_or_default)
	}

	/// Store a reply to a root where the relation walk reads it.
	///
	/// The relation row and the user's notification record are written with it.
	fn store_reply(
		services: &Services,
		(root, root_count): (&EventId, u64),
		(event_id, count): (&EventId, u64),
		rel_type: &str,
		actions: &[Action],
	) {
		let (user, room_id, _) = ids();
		let content = json!({
			"msgtype": "m.text", "body": "reply",
			"m.relates_to": { "rel_type": rel_type, "event_id": root },
		});

		store_pdu(services, room_id, count, event_id, &content);
		services
			.pdu_metadata
			.add_relation(Count::Normal(count), Count::Normal(root_count));

		notify(&services.pusher.db.useridcount_notification, user, count, actions);
	}

	/// Store an event where the timeline and the relation walk read it.
	///
	/// Only the event-id and PDU rows are written, as backfill would; the
	/// caller adds any relation.
	fn store_pdu(
		services: &Services,
		room_id: &RoomId,
		count: u64,
		event_id: &EventId,
		content: &Value,
	) {
		let pdu_id: RawPduId = PduId {
			shortroomid: SHORTROOMID,
			count: Count::Normal(count),
		}
		.into();

		let pdu = json!({
			"type": "m.room.message", "event_id": event_id, "room_id": room_id,
			"sender": "@bob:localhost", "origin_server_ts": 1, "depth": 1,
			"hashes": { "sha256": "hash" }, "prev_events": [], "auth_events": [],
			"content": content,
		});

		services.db["eventid_pduid"].insert(event_id.as_bytes(), pdu_id.as_bytes());
		services.db["pduid_pdu"].raw_put(pdu_id, Json(&pdu));
	}
}
