use std::{cmp::Ordering, collections::BTreeMap};

use futures::{
	FutureExt, StreamExt,
	future::{join, join3},
};
use ruma::{
	OwnedRoomId, RoomId, UInt, api::client::sync::sync_events::v5::ListId,
	events::room::member::MembershipState, uint,
};
use tuwunel_core::{
	Result, apply, debug_error, is_true,
	matrix::PduCount,
	trace,
	utils::{
		BoolExt, ReadyExt,
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

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn selector(
	conn: &mut Connection,
	sync_info: SyncInfo<'_>,
) -> (Window, ResponseLists) {
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

	rooms.sort_unstable_by(room_sort);
	for room in &rooms {
		conn.rooms
			.entry(room.room_id.clone())
			.or_default();
	}

	trace!(?rooms);
	let lists = response_lists(rooms.iter());

	trace!(?lists);
	let window = window(sync_info, conn, rooms.iter(), &lists, invites_blocked).await;

	trace!(?window);
	(window, lists)
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

	let selected = matched
		|| conn.subscriptions.contains_key(&room_id)
		|| matches!(membership, Some(MembershipState::Leave | MembershipState::Ban));

	if !selected {
		return None;
	}

	let membership_only = membership_only(membership.as_ref());

	let last_notification = async {
		if membership_only {
			return ActivityProbe::default();
		}

		let result = services
			.pusher
			.last_notification_read(sender_user, &room_id)
			.await;

		activity_probe(result, &room_id, "notification read", true)
	};

	let last_timeline = async {
		if membership_only {
			return ActivityProbe::default();
		}

		let result = services
			.timeline
			.last_timeline_count(None, &room_id, Some(conn.next_batch.into()))
			.await
			.map(PduCount::into_unsigned);

		activity_probe(result, &room_id, "timeline", false)
	};

	let last_membership = async {
		let result = match &membership {
			| Some(MembershipState::Join) => Some(
				services
					.state_cache
					.get_joined_count(&room_id, sender_user)
					.await,
			),
			| Some(MembershipState::Invite) => Some(
				services
					.state_cache
					.get_invite_count(&room_id, sender_user)
					.await,
			),
			| Some(MembershipState::Knock) => Some(
				services
					.state_cache
					.get_knock_count(&room_id, sender_user)
					.await,
			),
			| Some(MembershipState::Leave | MembershipState::Ban) => Some(
				services
					.state_cache
					.get_left_count(&room_id, sender_user)
					.await,
			),
			| _ => None,
		};

		result.map_or_else(ActivityProbe::default, |result| {
			activity_probe(result, &room_id, "membership", false)
		})
	};

	let (last_timeline, last_notification, last_membership) =
		join3(last_timeline, last_notification, last_membership).await;

	let (event_count, payload_count) =
		activity_counts(membership.as_ref(), conn.next_batch, ActivityCounts {
			timeline: last_timeline.count,
			notification: last_notification.count,
			membership: last_membership.count,
			payload_probe_failed: last_timeline.failed
				|| last_notification.failed
				|| last_membership.failed,
		});

	Some(WindowRoom {
		room_id,
		membership,
		lists,
		event_count,
		payload_count,
	})
}

#[derive(Copy, Clone, Default)]
struct ActivityProbe {
	count: Option<u64>,
	failed: bool,
}

#[derive(Copy, Clone)]
struct ActivityCounts {
	timeline: Option<u64>,
	notification: Option<u64>,
	membership: Option<u64>,
	payload_probe_failed: bool,
}

fn activity_counts(
	membership: Option<&MembershipState>,
	next_batch: u64,
	counts: ActivityCounts,
) -> (u64, u64) {
	let timeline = bounded_count(next_batch, counts.timeline);
	let notification = bounded_count(next_batch, counts.notification);
	let membership_count = bounded_count(next_batch, counts.membership);
	let membership_only = membership_only(membership);
	let event_count = if membership_only {
		membership_count
	} else {
		timeline.max(membership_count)
	};
	let fresh_count = if membership_only {
		membership_count
	} else {
		timeline.max(notification).max(membership_count)
	};
	let payload_count = if counts.payload_probe_failed {
		next_batch
	} else {
		fresh_count
	};

	(event_count, payload_count)
}

fn activity_probe(
	result: Result<u64>,
	room_id: &RoomId,
	source: &'static str,
	missing_is_zero: bool,
) -> ActivityProbe {
	match result {
		| Ok(count) => ActivityProbe { count: Some(count), failed: false },
		| Err(error) if missing_is_zero && error.is_not_found() => ActivityProbe::default(),
		| Err(error) => {
			debug_error!(%room_id, %source, "Failed to read sliding sync activity: {error}");
			ActivityProbe { count: None, failed: true }
		},
	}
}

#[inline]
fn membership_only(membership: Option<&MembershipState>) -> bool {
	matches!(
		membership,
		Some(
			MembershipState::Invite
				| MembershipState::Knock
				| MembershipState::Leave
				| MembershipState::Ban
		)
	)
}

fn bounded_count(next_batch: u64, count: Option<u64>) -> u64 {
	count
		.filter(|count| next_batch.ge(count))
		.unwrap_or_default()
}

#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(rooms = rooms.clone().count())
)]
async fn window<'a, Rooms>(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	rooms: Rooms,
	lists: &ResponseLists,
	invites_blocked: bool,
) -> Window
where
	Rooms: Iterator<Item = &'a WindowRoom> + Clone + Send + Sync,
{
	static FULL_RANGE: (UInt, UInt) = (UInt::MIN, UInt::MAX);

	let SyncInfo { services, sender_user, .. } = sync_info;

	let selections = lists
		.keys()
		.filter_map(|id| conn.lists.get(id).map(|list| (id, list)))
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
				.map(move |range| (id, range))
		})
		.flat_map(|(id, (start, end))| {
			select_list_range(rooms.clone(), id, start, end)
				.map(|room| (room.room_id.clone(), room.clone()))
		})
		.stream();

	let indexed_subscriptions = (conn.subscriptions.len() > 1).then(|| {
		rooms
			.clone()
			.filter(|room| conn.subscriptions.contains_key(&room.room_id))
			.map(|room| (&room.room_id, room))
			.collect::<BTreeMap<_, _>>()
	});

	let retractions = rooms
		.clone()
		.filter(|room| {
			matches!(room.membership, Some(MembershipState::Leave | MembershipState::Ban))
				&& conn
					.rooms
					.get(&room.room_id)
					.is_some_and(|conn_room| {
						conn_room.roomsince != 0 && room.payload_count > conn_room.roomsince
					})
		})
		.map(|room| (room.room_id.clone(), detached_room(room.clone())))
		.stream();

	let subscriptions = conn
		.subscriptions
		.iter()
		.stream()
		.broad_filter_map(async |(room_id, _)| {
			let room = indexed_subscriptions
				.as_ref()
				.map_or_else(
					|| {
						rooms
							.clone()
							.find(|room| room.room_id == *room_id)
					},
					|indexed| indexed.get(&room_id).copied(),
				)
				.cloned();

			let (membership, filter) = if let Some(room) = &room {
				(room.membership.clone(), filter_room_meta(sync_info, room_id).await)
			} else {
				join(
					services
						.state_cache
						.user_membership(sender_user, room_id),
					filter_room_meta(sync_info, room_id),
				)
				.await
			};

			// MSC4380: suppress invited-room subscriptions when invites are blocked.
			let suppress = invites_blocked && matches!(membership, Some(MembershipState::Invite));

			if !filter || suppress {
				return None;
			}

			if let Some(room) = room {
				return Some(detached_room(room));
			}

			matcher(sync_info, conn, room_id.clone(), membership)
				.await
				.map(detached_room)
		})
		.map(|room| (room.room_id.clone(), room));

	retractions
		.chain(subscriptions)
		.chain(selections)
		.collect()
		.await
}

fn detached_room(room: WindowRoom) -> WindowRoom { WindowRoom { lists: ListIds::new(), ..room } }

fn select_list_range<'a, Rooms>(
	rooms: Rooms,
	id: &ListId,
	start: usize,
	end: usize,
) -> impl Iterator<Item = &'a WindowRoom>
where
	Rooms: Iterator<Item = &'a WindowRoom>,
{
	rooms
		.filter(move |room| room.lists.contains(id))
		.filter(|room| {
			!matches!(room.membership, Some(MembershipState::Leave | MembershipState::Ban))
		})
		.skip(start)
		.take(end.saturating_add(1).saturating_sub(start))
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

fn room_sort(a: &WindowRoom, b: &WindowRoom) -> Ordering {
	b.event_count
		.cmp(&a.event_count)
		.then_with(|| a.room_id.cmp(&b.room_id))
}

#[cfg(test)]
mod tests {
	use ruma::{
		OwnedRoomId, api::client::sync::sync_events::v5::ListId,
		events::room::member::MembershipState, room_id,
	};

	use super::{
		ActivityCounts, ListIds, WindowRoom, activity_counts, detached_room, room_sort,
		select_list_range,
	};

	const NEXT_BATCH: u64 = 10;

	#[test]
	fn notification_activity_refreshes_payload_without_reordering() {
		let (event_count, payload_count) =
			activity_counts(Some(&MembershipState::Join), NEXT_BATCH, ActivityCounts {
				timeline: Some(2),
				notification: Some(8),
				membership: Some(1),
				payload_probe_failed: false,
			});

		assert_eq!((event_count, payload_count), (2, 8));

		let mut rooms = [
			room(room_id!("!current:example.com").to_owned(), 3, 3, None),
			room(
				room_id!("!notification:example.com").to_owned(),
				event_count,
				payload_count,
				None,
			),
		];

		rooms.sort_by(room_sort);
		assert_eq!(rooms[0].room_id, room_id!("!current:example.com"));
	}

	#[test]
	fn stripped_membership_rooms_use_only_membership_activity() {
		for membership in [
			MembershipState::Invite,
			MembershipState::Knock,
			MembershipState::Leave,
			MembershipState::Ban,
		] {
			let counts = activity_counts(Some(&membership), NEXT_BATCH, ActivityCounts {
				timeline: Some(9),
				notification: Some(8),
				membership: Some(4),
				payload_probe_failed: false,
			});

			assert_eq!(counts, (4, 4));
		}
	}

	#[test]
	fn activity_probe_failure_forces_payload_without_reordering() {
		let counts = activity_counts(Some(&MembershipState::Join), NEXT_BATCH, ActivityCounts {
			timeline: Some(2),
			notification: None,
			membership: Some(1),
			payload_probe_failed: true,
		});

		assert_eq!(counts, (2, NEXT_BATCH));
	}

	#[test]
	fn full_list_range_ignores_payload_freshness_and_departed_rooms() {
		let list = ListId::from("main");
		let mut rooms = [
			room(
				room_id!("!departed:example.com").to_owned(),
				40,
				40,
				Some(MembershipState::Leave),
			),
			room(room_id!("!stale:example.com").to_owned(), 30, 1, None),
			room(room_id!("!fresh:example.com").to_owned(), 20, 20, None),
			room(room_id!("!tail:example.com").to_owned(), 10, 10, None),
		];

		for room in &mut rooms {
			room.lists.push(list.clone());
		}

		let selected = select_list_range(rooms.iter(), &list, 0, 1)
			.map(|room| room.room_id.clone())
			.collect::<Vec<_>>();

		assert_eq!(selected, [
			room_id!("!stale:example.com").to_owned(),
			room_id!("!fresh:example.com").to_owned(),
		]);
	}

	#[test]
	fn equal_activity_rooms_have_deterministic_order() {
		let mut rooms = [
			room(room_id!("!z:example.com").to_owned(), 5, 5, None),
			room(room_id!("!a:example.com").to_owned(), 5, 5, None),
		];

		rooms.sort_by(room_sort);

		assert_eq!(rooms.map(|room| room.room_id), [
			room_id!("!a:example.com").to_owned(),
			room_id!("!z:example.com").to_owned(),
		]);
	}

	#[test]
	fn subscription_keeps_computed_payload_freshness() {
		let mut room = room(room_id!("!subscribed:example.com").to_owned(), 7, 9, None);
		room.lists.push(ListId::from("outside-range"));

		let room = detached_room(room);

		assert_eq!(room.payload_count, 9);
		assert!(room.lists.is_empty());
	}

	#[test]
	fn retraction_has_no_list_provenance() {
		let mut room = room(
			room_id!("!departed:example.com").to_owned(),
			7,
			9,
			Some(MembershipState::Leave),
		);

		room.lists
			.push(ListId::from("matched-before-leave"));

		let room = detached_room(room);

		assert!(room.lists.is_empty());
		assert_eq!(room.membership, Some(MembershipState::Leave));
	}

	fn room(
		room_id: OwnedRoomId,
		event_count: u64,
		payload_count: u64,
		membership: Option<MembershipState>,
	) -> WindowRoom {
		WindowRoom {
			room_id,
			membership,
			lists: ListIds::new(),
			event_count,
			payload_count,
		}
	}
}
