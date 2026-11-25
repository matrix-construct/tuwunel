use futures::{Stream, StreamExt, TryFutureExt, pin_mut};
use ruma::{UserId, events::ignored_user_list::IgnoredUserListEventContent};
use tuwunel_core::{
	Result,
	arrayvec::ArrayVec,
	implement,
	result::LogErr,
	utils::{
		BoolExt,
		stream::{ReadyExt, TryIgnore},
		u64_from_u8,
	},
};
use tuwunel_matrix::{Event, Pdu, PduCount, PduId, RawPduId};

use super::{
	Service, is_ignored_event,
	typed_relations::{CHILD_COUNT_OFFSET, KEY_LEN, Tag, prefix},
};
use crate::users::is_ignored;

type Seek = ArrayVec<u8, KEY_LEN>;

/// Fold read-time bundled aggregations into a served event's `unsigned`,
/// per-requester.
///
/// MSC3440: the stored `m.thread` bundle is rebuilt for a
/// `sender_user` who ignores a participant or may not see its latest reply.
/// MSC3816: the bundle's shared `current_user_participated` is recomputed for
/// `sender_user`. MSC3925:
/// when `bundle_edit_relations` is enabled, the newest `m.replace` edit is
/// folded in as the full replacement event, and the bundled thread
/// `latest_event` carries its own newest edit (MSC3856). MSC3267: when
/// `bundle_reference_relations` is enabled, the `m.reference` children are
/// folded in as a `{ chunk: [{ event_id }] }` summary. The thread presence gate
/// keeps the common no-bundle case to a substring scan; the edit and reference
/// folds are skipped unless enabled.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn bundle_aggregations(&self, sender_user: &UserId, mut pdu: Pdu) -> Pdu {
	// MSC4025: an erased sender's event serves as the pruned clone, and a
	// pruned event carries no aggregations.
	if let Some(pruned) = self
		.services
		.state_accessor
		.erased_view(sender_user, &pdu)
		.await
	{
		return pruned;
	}

	self.bundle_thread_for(sender_user, &mut pdu)
		.await;

	let replacement = self
		.services
		.server
		.config
		.rooms
		.relations
		.bundle_edits
		.then_async(|| self.newest_replacement(sender_user, &pdu))
		.await
		.flatten();

	if let Some(replacement) = replacement {
		let replacement = replacement.without_private_unless_sender(Some(sender_user));

		pdu.set_replacement_bundle(&replacement.into_format())
			.log_err()
			.ok();
	}

	let references = self
		.services
		.server
		.config
		.rooms
		.relations
		.bundle_references
		.then_async(|| self.references(&pdu))
		.await
		.unwrap_or_default();

	if !references.is_empty() {
		pdu.set_reference_bundle(&references)
			.log_err()
			.ok();
	}

	pdu
}

/// Readies an event for a requester on a path that folds no aggregations.
///
/// MSC4025: when this requester must receive an erased sender's event pruned,
/// the pruned clone is served, as in [`Service::bundle_aggregations`], or the
/// pruning error is returned rather than the event served intact. Any other
/// event loses its stored aggregations, which copy replies the requester may
/// not see, and another sender's private fields.
#[implement(Service)]
pub async fn unbundled(&self, sender_user: &UserId, pdu: Pdu) -> Result<Pdu> {
	self.services
		.state_accessor
		.try_erased_view(sender_user, &pdu)
		.await
		.map(|pruned| pruned.unwrap_or_else(|| pdu.served_unfolded_to(Some(sender_user))))
}

#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn bundle_thread_for(&self, sender_user: &UserId, pdu: &mut Pdu) {
	if pdu
		.has_thread_bundle()
		.log_err()
		.unwrap_or(true)
		.is_false()
	{
		return;
	}

	if pdu
		.thread_latest_event()
		.log_err()
		.ok()
		.flatten()
		.is_none()
		|| pdu
			.remove_thread_latest_transaction_id_unless_sender(sender_user)
			.log_err()
			.is_err()
	{
		drop_thread_bundle(pdu);
		return;
	}

	let view = self.thread_view(sender_user, pdu).await;

	if apply_thread_view(pdu, view).is_none() {
		return;
	}

	let own_latest = self.own_thread_latest(sender_user, pdu).await;

	if apply_own_thread_latest(pdu, own_latest).is_none() {
		return;
	}

	let participated = self
		.services
		.threads
		.user_participated(pdu.event_id(), sender_user)
		.await;

	let participation = pdu
		.set_thread_participated(participated)
		.log_err();

	if thread_result_or_drop(pdu, participation).is_none() {
		return;
	}

	self.erase_thread_latest(sender_user, pdu).await;

	let _: Option<()> = self
		.services
		.server
		.config
		.rooms
		.relations
		.bundle_edits
		.then_async(|| self.bundle_thread_latest_edit(sender_user, pdu))
		.await;
}

/// The requester's view of the stored thread summary (MSC3440, MSC2675).
///
/// The stored `latest_event` and `count` cover every reply. A requester who
/// ignores a participant, or who may not see the latest reply, gets them
/// rebuilt from a bounded walk of the newest replies: the result is how many
/// replies the walk hid, and the newest reply they may see when the stored
/// latest is hidden. `None` drops the summary: no reply they may see is in
/// reach, or the summary or its root cannot be read.
#[implement(Service)]
// Unspanned: span frames on this path overflow the debug-build worker stack.
async fn thread_view(&self, sender_user: &UserId, pdu: &Pdu) -> Option<(usize, Option<Pdu>)> {
	let (latest_id, _) = pdu
		.thread_latest_event()
		.log_err()
		.ok()
		.flatten()?;

	let ignored = self
		.services
		.users
		.ignored_users(sender_user)
		.await;

	let ignored = ignored.as_ref();

	let latest_shown = match self.services.timeline.get_pdu(&latest_id).await {
		| Err(_) => false,
		| Ok(latest) => self.shown_to(sender_user, ignored, &latest).await,
	};

	let ignores_nobody = ignored.is_none_or(|list| list.ignored_users.is_empty());

	if latest_shown && ignores_nobody {
		return Some((0, None));
	}

	let root_id = self
		.services
		.timeline
		.get_pdu_id(pdu.event_id())
		.await
		.ok()?;

	if latest_shown && !self.ignores_participant(ignored, &root_id).await {
		return Some((0, None));
	}

	let (hidden, newest_shown) = self
		.walk_replies(sender_user, ignored, root_id)
		.await;

	match latest_shown {
		| true => Some((hidden, None)),
		| false => newest_shown.map(|reply| (hidden, Some(reply))),
	}
}

/// Applies a requester's view to the served thread summary.
///
/// An unchanged view serves the stored summary as is. Otherwise the swapped-in
/// reply, without the aggregations stored on it, becomes the latest event and
/// the count loses the hidden replies. The summary is dropped, never served
/// with a count alone, for no view, a missing or used-up count, or a failed
/// rewrite; `None` then tells the caller to stop.
fn apply_thread_view(pdu: &mut Pdu, view: Option<(usize, Option<Pdu>)>) -> Option<()> {
	let Some((hidden, latest)) = view else {
		drop_thread_bundle(pdu);
		return None;
	};

	if hidden == 0 && latest.is_none() {
		return Some(());
	}

	if let Some(latest) = latest {
		replace_thread_latest(pdu, latest.without_aggregations())?;
	}

	let count = pdu.thread_count().log_err();
	let count = thread_result_or_drop(pdu, count)?
		.map(|count| count.saturating_sub(hidden))
		.filter(|&count| count > 0);

	let Some(count) = count else {
		drop_thread_bundle(pdu);
		return None;
	};

	let counted = pdu.set_thread_count(count).log_err();

	thread_result_or_drop(pdu, counted)
}

/// The requester's own latest reply, reloaded with its private fields.
///
/// The stored latest event carries none of its sender's private fields (the
/// transaction ID, MSC4140's delay ID), nor does a reply the requester's view
/// swaps in. A requester who sent the latest reply is served it unfolded: those
/// fields kept, the aggregations stored on it dropped. `None` for anyone else
/// keeps the copy in place. An unreadable summary or a reply that fails to load
/// is an error, since its sender may not be served the copy without its delay
/// ID (MSC4140).
#[implement(Service)]
// Unspanned: span frames on this path overflow the debug-build worker stack.
async fn own_thread_latest(&self, sender_user: &UserId, pdu: &Pdu) -> Result<Option<Pdu>> {
	let Some((event_id, _)) = pdu
		.thread_latest_event()
		.log_err()?
		.filter(|(_, sender)| sender.eq(sender_user))
	else {
		return Ok(None);
	};

	self.services
		.timeline
		.get_pdu(&event_id)
		.map_ok(|latest| latest.served_unfolded_to(Some(sender_user)))
		.map_ok(Some)
		.await
		.log_err()
}

/// Applies the requester's own latest reply to the served thread summary.
///
/// No reply of theirs serves the summary as is. An error or a failed rewrite
/// drops it, never serving the sender a copy without their private fields, and
/// `None` then tells the caller to stop.
fn apply_own_thread_latest(pdu: &mut Pdu, latest: Result<Option<Pdu>>) -> Option<()> {
	thread_result_or_drop(pdu, latest)?
		.map_or(Some(()), |latest| replace_thread_latest(pdu, latest))
}

/// Serves `latest` as the thread summary's latest event.
///
/// A failed rewrite drops the summary, never serving the copy it replaced, and
/// `None` then tells the caller to stop.
fn replace_thread_latest(pdu: &mut Pdu, latest: Pdu) -> Option<()> {
	let replaced = pdu
		.set_thread_latest_event(&latest.into_format())
		.log_err();

	thread_result_or_drop(pdu, replaced)
}

/// Whether one requester may be shown `pdu` inside an aggregation.
///
/// An event the requester's ignore list hides never is (MSC2675); otherwise
/// the room's history visibility decides. The ignore test runs first because
/// it reads nothing.
#[implement(Service)]
// Unspanned: span frames on this path overflow the debug-build worker stack.
async fn shown_to(
	&self,
	sender_user: &UserId,
	ignored: Option<&IgnoredUserListEventContent>,
	pdu: &Pdu,
) -> bool {
	!is_ignored_event(ignored, pdu)
		&& self
			.services
			.state_accessor
			.user_can_see_event(sender_user, pdu)
			.await
}

/// Whether the ignore list names any participant of a thread.
///
/// Unreadable participants count as ignored, so the summary is rebuilt rather
/// than served unfiltered.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn ignores_participant(
	&self,
	ignored: Option<&IgnoredUserListEventContent>,
	root_id: &RawPduId,
) -> bool {
	self.services
		.threads
		.get_participants(root_id)
		.await
		.ok()
		.is_none_or(|participants| {
			participants
				.iter()
				.any(|user| is_ignored(ignored, user))
		})
}

/// Walks a thread's newest replies within the summary search limit.
///
/// Returns how many the requester may not be shown, and the newest one they
/// may. A reply that fails to load is skipped. The walk is sequential because
/// callers already serve events concurrently.
#[implement(Service)]
// Unspanned: span frames on this path overflow the debug-build worker stack.
// size firewall
#[cfg_attr(debug_assertions, tuwunel_core::async_noinline)]
async fn walk_replies<'a>(
	&'a self,
	sender_user: &'a UserId,
	ignored: Option<&'a IgnoredUserListEventContent>,
	root_id: RawPduId,
) -> (usize, Option<Pdu>) {
	let limit = self
		.services
		.server
		.config
		.rooms
		.relations
		.thread_summary_search_limit;

	self.services
		.threads
		.thread_replies(root_id, limit)
		.ignore_err()
		.fold((0, None), async |(hidden, newest): (usize, Option<Pdu>), (_, reply)| {
			let shown = self.shown_to(sender_user, ignored, &reply).await;

			match shown {
				| true => (hidden, newest.or(Some(reply))),
				| false => (hidden.saturating_add(1), newest),
			}
		})
		.await
}

/// MSC4025: the stored thread bundle carries a full `latest_event` of any
/// sender; an erased hit swaps in the pruned form for this recipient. The
/// event load and membership check run only on the erased hit.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn erase_thread_latest(&self, sender_user: &UserId, pdu: &mut Pdu) {
	let identity = pdu.thread_latest_event().log_err();

	let Some((event_id, sender)) = thread_result_or_drop(pdu, identity).flatten() else {
		return;
	};

	if !self.services.users.is_erased(&sender).await {
		return;
	}

	let latest = self.services.timeline.get_pdu(&event_id).await;
	let Some(latest) = thread_result_or_drop(pdu, latest) else {
		return;
	};

	let Some(pruned) = self
		.services
		.state_accessor
		.erased_view(sender_user, &latest)
		.await
	else {
		return;
	};

	replace_thread_latest(pdu, pruned);
}

fn thread_result_or_drop<T, E>(pdu: &mut Pdu, result: Result<T, E>) -> Option<T> {
	result
		.inspect_err(|_| drop_thread_bundle(pdu))
		.ok()
}

fn drop_thread_bundle(pdu: &mut Pdu) { pdu.remove_thread_bundle().log_err().ok(); }

/// The thread module's aggregated `latest_event` (MSC3856): when the edit
/// fold is enabled, the bundled latest reply carries its own newest
/// `m.replace` edit, so thread previews track edits. Erased-sender bundles
/// stay in their pruned form.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn bundle_thread_latest_edit(&self, sender_user: &UserId, pdu: &mut Pdu) {
	let identity = pdu.thread_latest_event().log_err();

	let Some((event_id, _)) = thread_result_or_drop(pdu, identity).flatten() else {
		return;
	};

	let latest_event = self
		.services
		.timeline
		.get_pdu(&event_id)
		.map_ok(|latest| latest.served_unfolded_to(Some(sender_user)))
		.await
		.log_err();

	let Some(mut latest_event) = thread_result_or_drop(pdu, latest_event) else {
		return;
	};

	if self
		.services
		.state_accessor
		.erased_for(sender_user, &latest_event)
		.await
	{
		return;
	}

	let Some(mut replacement_event) = self
		.newest_replacement(sender_user, &latest_event)
		.await
	else {
		return;
	};

	replacement_event.remove_private_unless_sender(Some(sender_user));

	let replacement = latest_event
		.set_replacement_bundle(&replacement_event.into_format())
		.log_err();

	if thread_result_or_drop(pdu, replacement).is_none() {
		return;
	}

	replace_thread_latest(pdu, latest_event);
}

/// MSC3925: the newest `m.replace` edit of `parent` as a full event, or `None`
/// when `parent` is redacted, has no valid edit, or the newest valid edit
/// is hidden from this requester. An edit counts only when it
/// shares the parent's sender and type and is not itself redacted; newest is by
/// `origin_server_ts`, which the typed index sorts on.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn newest_replacement(&self, sender_user: &UserId, parent: &Pdu) -> Option<Pdu> {
	if parent.is_redacted() {
		return None;
	}

	let parent_id: PduId = self
		.services
		.timeline
		.get_pdu_id(parent.event_id())
		.map_ok(Into::into)
		.await
		.ok()?;

	let replacements = self.replacement_children(parent, parent_id);

	pin_mut!(replacements);
	let edit = replacements.next().await?;
	let access = &self.services.state_accessor;

	if access.erased_for(sender_user, &edit).await
		|| !access
			.user_can_see_event(sender_user, &edit)
			.await
	{
		return None;
	}

	Some(edit)
}

/// Stream `parent`'s valid `m.replace` children, newest `origin_server_ts`
/// first, from the typed index. A child counts only when it shares the parent's
/// sender and type and is not itself redacted.
#[implement(Service)]
fn replacement_children<'a>(
	&'a self,
	parent: &'a Pdu,
	parent_id: PduId,
) -> impl Stream<Item = Pdu> + Send + 'a {
	let shortroomid = parent_id.shortroomid;
	let prefix = prefix(shortroomid, parent_id.count, Tag::Replace);

	let mut seek = Seek::new();

	seek.extend(prefix.iter().copied());
	seek.extend([u8::MAX; size_of::<u64>() * 2]);

	self.db
		.relatesto_typed
		.rev_raw_keys_from(seek.as_slice())
		.ignore_err()
		.ready_take_while(move |key| key.starts_with(&prefix))
		.map(|key| u64_from_u8(&key[CHILD_COUNT_OFFSET..KEY_LEN]))
		.map(PduCount::from_unsigned)
		.map(move |count| (shortroomid, count))
		.filter_map(async |(shortroomid, count)| {
			let child_id: RawPduId = PduId { shortroomid, count }.into();
			self.services
				.timeline
				.get_pdu_from_id(&child_id)
				.await
				.ok()
				.filter(|child| !child.is_redacted())
				.filter(|child| child.sender() == parent.sender())
				.filter(|child| child.kind() == parent.kind())
		})
}

/// The spec'd redacted form of a thread root whose sender the requester ignores.
///
/// Content side only (MSC3856). `None` when the ignore list does not hide the
/// root, or on a redaction failure, since serving it unredacted then matches the
/// reference implementation.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn redacted_root(
	&self,
	ignored: Option<&IgnoredUserListEventContent>,
	root: &Pdu,
) -> Option<Pdu> {
	is_ignored_event(ignored, root)
		.then_async(async || {
			self.services
				.state
				.get_room_version_rules(root.room_id())
				.await
				.log_err()
				.ok()
				.and_then(|rules| root.redacted(&rules.redaction).log_err().ok())
		})
		.await
		.flatten()
}

#[cfg(test)]
mod tests {
	use serde_json::{Value, from_str, from_value, json};

	use super::*;

	#[test]
	fn thread_latest_load_error_drops_bundle() {
		let mut pdu = member_event(&json!({
			"age": 4612,
			"m.relations": {
				"m.replace": { "event_id": "$edit:example.com" },
				"m.thread": { "count": 3 },
			},
		}));

		let latest: Option<()> = thread_result_or_drop(&mut pdu, Err("missing latest event"));

		assert!(latest.is_none(), "load failure returned a latest event");

		let unsigned = unsigned_of(&pdu);

		assert!(
			unsigned["m.relations"].get("m.thread").is_none(),
			"failed load retained the thread bundle",
		);

		assert_eq!(
			unsigned["m.relations"]["m.replace"],
			json!({ "event_id": "$edit:example.com" }),
			"failed load removed a sibling relation",
		);
		assert_eq!(unsigned["age"], 4612, "failed load removed outer unsigned data");
	}

	#[test]
	fn swapped_latest_carries_no_stored_aggregations() {
		let hidden = json!({ "event_id": "$hidden:example.com" });
		let mut root = member_event(&json!({
			"m.relations": { "m.thread": { "count": 2, "latest_event": hidden } },
		}));

		// A reply rooting a thread of its own, as an older release allowed.
		let latest = member_event(&json!({ "m.relations": { "m.thread": { "count": 1 } } }));

		assert!(apply_thread_view(&mut root, Some((1, Some(latest)))).is_some());

		let thread = &unsigned_of(&root)["m.relations"]["m.thread"];

		assert_eq!(thread["count"], 1);
		assert_eq!(thread["latest_event"]["event_id"], "$member:example.com");
		assert!(
			thread["latest_event"].get("unsigned").is_none(),
			"the swapped-in reply kept its stored aggregations",
		);
	}

	fn member_event(unsigned: &Value) -> Pdu {
		from_value(json!({
			"type": "m.room.member",
			"content": { "membership": "join" },
			"event_id": "$member:example.com",
			"room_id": "!room:example.com",
			"sender": "@alice:example.com",
			"state_key": "@alice:example.com",
			"prev_events": ["$prev:example.com"],
			"auth_events": ["$auth:example.com"],
			"origin_server_ts": 1_838_188_000,
			"depth": 12,
			"hashes": { "sha256": "thishashcoversallfieldsincasethisisredacted" },
			"unsigned": unsigned,
		}))
		.expect("test fixture should deserialize as a valid PDU")
	}

	fn unsigned_of(pdu: &Pdu) -> Value {
		let unsigned = pdu
			.unsigned
			.as_ref()
			.expect("unsigned data should remain");

		from_str(unsigned.json().get()).expect("unsigned data should be valid JSON")
	}
}
