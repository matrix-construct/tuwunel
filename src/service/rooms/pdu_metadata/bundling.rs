use std::collections::BTreeSet;

use futures::{Stream, StreamExt, TryFutureExt, pin_mut};
use ruma::{OwnedUserId, UserId, api::Direction, events::room::encrypted::Relation};
use tuwunel_core::{
	PduId,
	arrayvec::ArrayVec,
	implement,
	matrix::{Event, Pdu, PduCount, RawPduId},
	result::LogErr,
	utils::{
		BoolExt,
		stream::{ReadyExt, TryIgnore},
		u64_from_u8,
	},
};

use super::{
	ExtractRelatesTo, IgnoredThreadView,
	IgnoredThreadView::{Adjusted, Omitted, Unchanged},
	Service,
	typed_relations::{CHILD_COUNT_OFFSET, KEY_LEN, Tag, prefix},
};

type Seek = ArrayVec<u8, KEY_LEN>;

/// Fold read-time bundled aggregations into a served event's `unsigned`,
/// per-requester. MSC3816: the stored `m.thread` bundle carries a shared
/// `current_user_participated`, recomputed here for `sender_user`. MSC3925:
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

	let has_thread = pdu
		.unsigned()
		.is_some_and(|unsigned| unsigned.get().contains("m.thread"));

	if has_thread {
		let participated = self
			.services
			.threads
			.user_participated(pdu.event_id(), sender_user)
			.await;

		pdu.set_thread_participated(participated)
			.log_err()
			.ok();

		self.erase_thread_latest(sender_user, &mut pdu)
			.await;

		if self.services.server.config.bundle_edit_relations {
			self.bundle_thread_latest_edit(sender_user, &mut pdu)
				.await;
		}
	}

	let replacement = self
		.services
		.server
		.config
		.bundle_edit_relations
		.then_async(|| self.newest_replacement(&pdu))
		.await
		.flatten();

	if let Some(replacement) = replacement
		&& !self
			.services
			.state_accessor
			.erased_for(sender_user, &replacement)
			.await
	{
		pdu.set_replacement_bundle(&replacement.into_format())
			.log_err()
			.ok();
	}

	let references = self
		.services
		.server
		.config
		.bundle_reference_relations
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

/// MSC4025: the stored thread bundle carries a full `latest_event` of any
/// sender; an erased hit swaps in the pruned form for this recipient. The
/// event load and membership check run only on the erased hit.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn erase_thread_latest(&self, sender_user: &UserId, pdu: &mut Pdu) {
	let Some((event_id, sender)) = pdu.thread_latest_event() else {
		return;
	};

	if !self.services.users.is_erased(&sender).await {
		return;
	}

	let Ok(latest) = self.services.timeline.get_pdu(&event_id).await else {
		return;
	};

	if let Some(pruned) = self
		.services
		.state_accessor
		.erased_view(sender_user, &latest)
		.await
	{
		pdu.set_thread_latest_event(&pruned.into_format())
			.log_err()
			.ok();
	}
}

/// The thread module's aggregated `latest_event` (MSC3856): when the edit
/// fold is enabled, the bundled latest reply carries its own newest
/// `m.replace` edit, so thread previews track edits. Erased-sender bundles
/// stay in their pruned form.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn bundle_thread_latest_edit(&self, sender_user: &UserId, pdu: &mut Pdu) {
	let Some((event_id, _)) = pdu.thread_latest_event() else {
		return;
	};

	let Ok(mut latest) = self.services.timeline.get_pdu(&event_id).await else {
		return;
	};

	if self
		.services
		.state_accessor
		.erased_for(sender_user, &latest)
		.await
	{
		return;
	}

	let Some(replacement) = self.newest_replacement(&latest).await else {
		return;
	};

	if self
		.services
		.state_accessor
		.erased_for(sender_user, &replacement)
		.await
	{
		return;
	}

	if latest
		.set_replacement_bundle(&replacement.into_format())
		.log_err()
		.is_err()
	{
		return;
	}

	pdu.set_thread_latest_event(&latest.into_format())
		.log_err()
		.ok();
}

/// MSC3925: the newest `m.replace` edit of `parent` as a full event, or `None`
/// when `parent` is redacted or has no valid edit. An edit counts only when it
/// shares the parent's sender and type and is not itself redacted; newest is by
/// `origin_server_ts`, which the typed index sorts on.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn newest_replacement(&self, parent: &Pdu) -> Option<Pdu> {
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
	replacements.next().await
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

/// MSC3856: evaluate one served thread root against the requester's ignore
/// list. A cheap participant intersection gates the reply walk; one walk then
/// yields the replacement `latest_event`, the ignored-aware `count`, and the
/// omit-when-every-reply-is-ignored verdict. A root whose replies are not
/// indexed (backfilled history) adjusts nothing beyond its own redacted form.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn ignored_thread_view(
	&self,
	sender_user: &UserId,
	ignored: &BTreeSet<OwnedUserId>,
	root: &Pdu,
) -> IgnoredThreadView {
	let Ok(root_id) = self
		.services
		.timeline
		.get_pdu_id(root.event_id())
		.await
	else {
		return Unchanged;
	};

	let participants = self
		.services
		.threads
		.get_participants(&root_id)
		.await
		.unwrap_or_default();

	if !participants
		.iter()
		.any(|user| ignored.contains(user))
	{
		return Unchanged;
	}

	let root_pid: PduId = root_id.into();
	let replies = self
		.get_relations(
			root_pid.shortroomid,
			root_pid.count,
			None,
			Direction::Backward,
			Some(sender_user),
		)
		.ready_filter_map(|(_, pdu)| {
			pdu.get_content()
				.is_ok_and(|content: ExtractRelatesTo| {
					matches!(content.relates_to, Relation::Thread(_))
				})
				.then_some(pdu)
		});

	let fold = |(total, unignored, latest): (usize, usize, Option<Pdu>), pdu: Pdu| match ignored
		.contains(pdu.sender())
	{
		| true => (total.saturating_add(1), unignored, latest),
		| false => (total.saturating_add(1), unignored.saturating_add(1), latest.or(Some(pdu))),
	};

	let (total, unignored, latest) = replies.ready_fold((0, 0, None), fold).await;

	if total == 0 {
		return match self.redacted_root(ignored, root).await {
			| None => Unchanged,
			| root => Adjusted { root, count: None, latest: None },
		};
	}

	if unignored == 0 {
		return Omitted;
	}

	let swap = root
		.thread_latest_event()
		.is_some_and(|(_, sender)| ignored.contains(&sender));

	let latest = match swap.then_some(latest).flatten() {
		| None => None,
		| Some(reply) => {
			// MSC4025: the swapped-in reply must not reopen the erased-sender
			// seam the bundle pass gates on the stored latest.
			let reply = self
				.services
				.state_accessor
				.erased_view(sender_user, &reply)
				.await
				.unwrap_or(reply);

			Some(reply.into_format())
		},
	};

	let count = unignored.ne(&total).then_some(unignored);

	let root = self.redacted_root(ignored, root).await;

	if root.is_none() && count.is_none() && latest.is_none() {
		return Unchanged;
	}

	Adjusted { root, count, latest }
}

/// The spec'd redacted form of an ignored sender's thread root, content side
/// only; `None` when the sender is not ignored, or on a redaction failure
/// (serving unredacted then matches the reference implementation).
#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn redacted_root(&self, ignored: &BTreeSet<OwnedUserId>, root: &Pdu) -> Option<Box<Pdu>> {
	ignored
		.contains(root.sender())
		.then_async(async || {
			self.services
				.state
				.get_room_version_rules(root.room_id())
				.await
				.log_err()
				.ok()
				.and_then(|rules| root.redacted(&rules.redaction).log_err().ok())
				.map(Box::new)
		})
		.await
		.flatten()
}
