use std::cmp::Ordering;

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, join3},
};
use ruma::{OwnedRoomId, UInt, events::room::member::MembershipState, uint};
use tuwunel_core::{
	apply, is_true,
	matrix::PduCount,
	trace,
	utils::{
		BoolExt, ReadyExt, TryFutureExtExt,
		math::usize_from_ruma,
		option::OptionExt,
		stream::{BroadbandExt, IterStream},
	},
};
use tuwunel_service::sync::Connection;

use super::{
	ListIds, ResponseLists, SyncInfo, Window, WindowRoom,
	filter::{filter_room, filter_room_meta},
};

#[cfg(test)]
mod tests;

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn selector(
	conn: &mut Connection,
	sync_info: SyncInfo<'_>,
) -> (Window, Option<Window>, ResponseLists) {
	use MembershipState::*;

	let SyncInfo { services, sender_user, .. } = sync_info;

	// MSC4380: when m.invite_permission_config blocks invites, omit invited
	// rooms from the sliding-sync window; an unblock re-exposes them.
	let invites_blocked = services.users.invites_blocked(sender_user).await;

	let actives = services
		.state_cache
		.user_memberships(sender_user, Some(&[Join, Invite, Knock]))
		.ready_filter(move |(m, _)| !invites_blocked || !matches!(m, Invite))
		.map(|(membership, room_id)| (room_id.to_owned(), Some(membership)));

	// Source retractions from tracked rooms, not a full left-state scan.
	let retracted = conn
		.rooms
		.keys()
		.stream()
		.broad_filter_map(async |room_id| {
			services
				.state_cache
				.is_left(sender_user, room_id)
				.await
				.then_some((room_id.clone(), Some(Leave)))
		});

	let mut rooms = actives
		.chain(retracted)
		.broad_filter_map(|(room_id, membership)| matcher(sync_info, conn, room_id, membership))
		.collect::<Vec<_>>()
		.await;

	rooms.sort_by(room_sort);
	rooms
		.iter_mut()
		.enumerate()
		.for_each(|(i, room)| {
			room.ranked = i;

			conn.rooms
				.entry(room.room_id.clone())
				.or_default();
		});

	trace!(?rooms);
	let lists = response_lists(rooms.iter());

	trace!(?lists);
	let gated_window = window(sync_info, conn, rooms.iter(), &lists, true, invites_blocked).await;

	let typing_window = conn
		.extensions
		.typing
		.enabled
		.unwrap_or(false)
		.then_async(|| window(sync_info, conn, rooms.iter(), &lists, false, invites_blocked))
		.await;

	trace!(?gated_window);
	(gated_window, typing_window, lists)
}

#[tracing::instrument(
	name = "matcher",
	level = "trace",
	skip_all,
	fields(?room_id, ?membership)
)]
async fn matcher(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	room_id: OwnedRoomId,
	membership: Option<MembershipState>,
) -> Option<WindowRoom> {
	let SyncInfo { services, sender_user, .. } = sync_info;

	let (matched, lists) = conn
		.lists
		.iter()
		.stream()
		.filter_map(async |(id, list)| {
			list.filters
				.clone()
				.map_async(async |filters| {
					filter_room(sync_info, &filters, &room_id, membership.as_ref()).await
				})
				.await
				.is_none_or(is_true!())
				.then(|| id.clone())
		})
		.collect::<ListIds>()
		.map(|lists| (lists.is_empty().is_false(), lists))
		.await;

	let last_notification = matched.then_async(|| {
		services
			.pusher
			.last_notification_read(sender_user, &room_id)
			.unwrap_or_default()
	});

	let last_privateread = matched.then_async(|| {
		services
			.read_receipt
			.last_privateread_update(sender_user, &room_id)
	});

	let last_receipt = matched.then_async(|| {
		services
			.read_receipt
			.last_receipt_count(&room_id, sender_user.into(), None)
			.unwrap_or_default()
	});

	let last_account = matched.then_async(|| {
		services
			.account_data
			.last_count(Some(room_id.as_ref()), sender_user, Some(conn.next_batch))
			.unwrap_or_default()
	});

	let last_timeline = matched.then_async(|| {
		services
			.timeline
			.last_timeline_count(None, &room_id, Some(conn.next_batch.into()))
			.map_ok(PduCount::into_unsigned)
			.unwrap_or_default()
	});

	let last_membership = matched.then_async(async || match &membership {
		| Some(MembershipState::Invite) => services
			.state_cache
			.get_invite_count(&room_id, sender_user)
			.await
			.unwrap_or_default(),
		| Some(MembershipState::Leave | MembershipState::Ban) => services
			.state_cache
			.get_left_count(&room_id, sender_user)
			.await
			.unwrap_or_default(),
		| _ => 0,
	});

	let (
		(last_timeline, last_notification, last_account),
		(last_receipt, last_privateread, last_membership),
	) = join(
		join3(last_timeline, last_notification, last_account),
		join3(last_receipt, last_privateread, last_membership),
	)
	.await;

	// A departed room surfaces only on its own leave count, never on room-global
	// timeline activity the user no longer receives, so the retraction is one-shot.
	let last_count = match &membership {
		| Some(MembershipState::Leave | MembershipState::Ban) => last_membership
			.filter(|count| conn.next_batch.ge(count))
			.unwrap_or_default(),
		| _ => [
			last_timeline,
			last_notification,
			last_account,
			last_receipt,
			last_privateread,
			last_membership,
		]
		.into_iter()
		.map(Option::unwrap_or_default)
		.filter(|count| conn.next_batch.ge(count))
		.max()
		.unwrap_or_default(),
	};

	Some(WindowRoom {
		room_id: room_id.clone(),
		membership,
		lists,
		ranked: 0,
		last_count,
	})
}

#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(rooms = rooms.clone().count(), gated)
)]
async fn window<'a, Rooms>(
	sync_info: SyncInfo<'_>,
	conn: &'a Connection,
	rooms: Rooms,
	lists: &'a ResponseLists,
	gated: bool,
	invites_blocked: bool,
) -> Window
where
	Rooms: Iterator<Item = &'a WindowRoom> + Clone + Send + Sync + 'a,
{
	let SyncInfo { services, sender_user, .. } = sync_info;

	let selections = list_selections(conn, rooms, lists, gated).stream();

	let subscriptions = conn
		.subscriptions
		.iter()
		.stream()
		.broad_filter_map(async |(room_id, _)| {
			let membership = services
				.state_cache
				.user_membership(sender_user, room_id);

			let filter = filter_room_meta(sync_info, room_id);

			let (membership, filter) = join(membership, filter).await;

			// MSC4380: suppress invited-room subscriptions when invites are blocked.
			let suppress = invites_blocked && matches!(membership, Some(MembershipState::Invite));

			(filter && !suppress).then(|| WindowRoom {
				room_id: room_id.clone(),
				lists: Default::default(),
				ranked: usize::MAX,
				last_count: 0,
				membership,
			})
		})
		.map(|room| (room.room_id.clone(), room));

	subscriptions.chain(selections).collect().await
}

fn list_selections<'a, Rooms>(
	conn: &'a Connection,
	rooms: Rooms,
	lists: &'a ResponseLists,
	gated: bool,
) -> impl Iterator<Item = (OwnedRoomId, WindowRoom)> + 'a
where
	Rooms: Iterator<Item = &'a WindowRoom> + Clone + Send + Sync + 'a,
{
	static FULL_RANGE: (UInt, UInt) = (UInt::MIN, UInt::MAX);

	lists
		.keys()
		.cloned()
		.filter_map(|id| conn.lists.get(&id).map(|list| (id, list)))
		.flat_map(|(id, list)| {
			let full_range = list
				.ranges
				.is_empty()
				.then_some(&FULL_RANGE)
				.into_iter();

			list.ranges
				.iter()
				.chain(full_range)
				.map(apply!(2, usize_from_ruma))
				.map(move |range| (id.clone(), range))
		})
		.flat_map(move |(id, (start, end))| {
			rooms
				.clone()
				.filter(move |&room| room.lists.contains(&id))
				.filter(move |&room| {
					!gated
						|| conn
							.rooms
							.get(&room.room_id)
							.is_some_and(|conn_room| {
								conn_room.roomsince == 0 || room.last_count > conn_room.roomsince
							})
				})
				.enumerate()
				.skip_while(move |&(i, _)| i < start)
				.take(end.saturating_add(1).saturating_sub(start))
				.map(|(_, room)| (room.room_id.clone(), room.clone()))
		})
}

fn response_lists<'a, Rooms>(rooms: Rooms) -> ResponseLists
where
	Rooms: Iterator<Item = &'a WindowRoom>,
{
	rooms
		.filter(|room| {
			!matches!(room.membership, Some(MembershipState::Leave | MembershipState::Ban))
		})
		.flat_map(|room| room.lists.iter())
		.fold(ResponseLists::default(), |mut lists, id| {
			let list = lists.entry(id.clone()).or_default();
			list.count = list
				.count
				.checked_add(uint!(1))
				.expect("list count must not overflow JsInt");

			lists
		})
}

fn room_sort(a: &WindowRoom, b: &WindowRoom) -> Ordering { b.last_count.cmp(&a.last_count) }
