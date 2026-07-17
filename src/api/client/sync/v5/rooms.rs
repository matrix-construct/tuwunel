mod bump_stamp;
mod heroes;

use std::collections::{BTreeMap, HashSet};

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, join3, join4},
};
use ruma::{
	JsOption, MxcUri, OwnedEventId, OwnedMxcUri, OwnedRoomId, RoomId, UInt, UserId,
	api::client::sync::sync_events::{
		UnreadNotificationsCount,
		v5::{DisplayName, response, response::Heroes},
	},
	events::{
		AnySyncStateEvent, StateEventType, TimelineEventType, room::member::MembershipState,
	},
	serde::Raw,
};
use tuwunel_core::{
	Result, at, err, error, is_equal_to,
	itertools::Itertools,
	matrix::{
		Event, StateKey,
		pdu::{PduCount, PduEvent},
	},
	ref_at,
	utils::{
		BoolExt, IterStream, ReadyExt, TryFutureExtExt,
		math::usize_from_ruma,
		result::FlatOk,
		stream::{BroadbandExt, WidebandExt},
	},
};
use tuwunel_service::{Services, sync::Room};

use self::{bump_stamp::room_bump_stamp, heroes::calculate_heroes};
use super::{
	super::{load_timeline, strip_prev_state},
	Connection, ListIds, SyncInfo, Window, WindowRoom,
};
use crate::client::{annotate_membership, ignored_filter, with_membership};

type ThreadCounts = BTreeMap<OwnedEventId, (u64, u64)>;

#[tracing::instrument(
    name = "rooms",
    level = "debug",
    skip_all,
    fields(
        next_batch = conn.next_batch,
        window = window.len(),
    )
)]
pub(super) async fn handle(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
) -> Result<BTreeMap<OwnedRoomId, response::Room>> {
	window
		.iter()
		.stream()
		.broad_filter_map(async |(room_id, room)| {
			handle_room(sync_info, conn, room)
				.map_ok(move |room| (room_id.clone(), room))
				.inspect_err(|e| error!(?room_id, "sync handler: {e:?}"))
				.await
				.ok()
		})
		.collect()
		.map(Ok)
		.await
}

#[tracing::instrument(
	name = "room",
	level = "debug",
	skip_all,
	fields(room_id, roomsince)
)]
async fn handle_room(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window_room: &WindowRoom,
) -> Result<response::Room> {
	let SyncInfo { services, sender_user, .. } = sync_info;
	let WindowRoom {
		lists, membership, room_id, last_count, ..
	} = window_room;

	let &Room { roomsince, .. } = conn
		.rooms
		.get(room_id)
		.ok_or_else(|| err!("Missing connection state for {room_id}"))?;

	debug_assert!(
		*last_count > roomsince || *last_count == 0 || roomsince == 0,
		"Stale room shouldn't be in the window"
	);

	if matches!(*membership, Some(MembershipState::Leave | MembershipState::Ban)) {
		return leave_or_ban_response(sync_info, conn, window_room, roomsince).await;
	}

	let is_invite = *membership == Some(MembershipState::Invite);

	let encrypted = services
		.state_accessor
		.is_encrypted_room(room_id)
		.await;

	let (timeline_limit, required_state) = merged_room_details(conn, lists, room_id);

	let timeline = is_invite.is_false().then_async(|| {
		load_timeline(
			services,
			sender_user,
			room_id,
			PduCount::Normal(roomsince),
			Some(PduCount::from(conn.next_batch)),
			timeline_limit,
		)
	});

	let (timeline_pdus, limited, last_timeline_count) = timeline
		.await
		.flat_ok()
		.unwrap_or_else(|| (Vec::new(), true, PduCount::default()));

	let required_state = required_state.into_iter().collect::<Vec<_>>();

	let prev_batch = timeline_pdus
		.first()
		.map(at!(0))
		.map(PduCount::into_unsigned)
		.as_ref()
		.map(ToString::to_string);

	let bump_stamp = room_bump_stamp(
		services,
		sender_user,
		room_id,
		PduCount::Normal(roomsince),
		PduCount::from(conn.next_batch),
		last_timeline_count,
	)
	.await;

	let num_live = roomsince
		.ne(&0)
		.and_is(limited || timeline_pdus.len() >= timeline_limit)
		.then_async(|| {
			services
				.timeline
				.pdus(None, room_id, Some(roomsince.into()))
				.count()
				.map(TryInto::try_into)
				.map(Result::ok)
		});

	let required_state = collect_required_state(
		services,
		sender_user,
		room_id,
		&required_state,
		&timeline_pdus,
		encrypted,
	);

	// TODO: figure out a timestamp we can use for remote invites
	let invite_state = is_invite.then_async(|| {
		services
			.state_cache
			.invite_state(sender_user, room_id)
			.ok()
	});

	let timeline = timeline_pdus
		.iter()
		.stream()
		.filter_map(|item| ignored_filter(services, item.clone(), sender_user))
		.map(at!(1))
		.wide_then(|pdu| with_membership(services, pdu, sender_user, encrypted))
		.wide_then(|pdu| {
			services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
		})
		.map(Event::into_format)
		.collect();

	let meta = room_meta_future(services, sender_user, room_id);
	let events = join4(timeline, num_live, required_state, invite_state);
	let member_counts = member_counts_future(services, room_id);
	let notification_counts = notification_counts_future(services, sender_user, room_id);
	let (
		(room_name, room_avatar, is_dm),
		(timeline, num_live, required_state, invite_state),
		(joined_count, invited_count),
		(highlight_count, notification_count, _last_notification_read, thread_counts),
	) = join4(meta, events, member_counts, notification_counts)
		.boxed()
		.await;

	let (heroes, heroes_name, heroes_avatar) = resolve_heroes(
		services,
		sender_user,
		room_id,
		room_name.as_ref(),
		room_avatar.as_deref(),
	)
	.await;

	Ok(response::Room {
		initial: roomsince.eq(&0).then_some(true),
		lists: lists.clone(),
		membership: membership.clone(),
		name: room_name.or(heroes_name),
		avatar: JsOption::from_option(room_avatar.or(heroes_avatar)),
		is_dm,
		heroes,
		required_state,
		invite_state: invite_state.flatten(),
		prev_batch: prev_batch.as_deref().map(Into::into),
		num_live: num_live.flatten(),
		limited,
		timeline,
		bump_stamp,
		joined_count,
		invited_count,
		unread_notifications: merge_unread_notifications(
			highlight_count,
			notification_count,
			&thread_counts,
		),
	})
}

async fn leave_or_ban_response(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	WindowRoom { lists, membership, room_id, .. }: &WindowRoom,
	roomsince: u64,
) -> Result<response::Room> {
	let member_event = services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, sender_user.as_str())
		.map_ok(Event::into_format)
		.await?;

	Ok(response::Room {
		initial: roomsince.eq(&0).then_some(true),
		lists: lists.clone(),
		membership: membership.clone(),
		prev_batch: Some(conn.next_batch.to_string().into()),
		limited: true,
		required_state: vec![member_event],
		..Default::default()
	})
}

fn merged_room_details(
	conn: &Connection,
	lists: &ListIds,
	room_id: &RoomId,
) -> (usize, HashSet<(StateEventType, StateKey)>) {
	lists
		.iter()
		.filter_map(|list_id| conn.lists.get(list_id))
		.map(|list| &list.room_details)
		.chain(conn.subscriptions.get(room_id))
		.fold((0_usize, HashSet::new()), |(timeline_limit, mut required_state), config| {
			required_state.extend(config.required_state.clone());
			(timeline_limit.max(usize_from_ruma(config.timeline_limit)), required_state)
		})
}

async fn resolve_heroes(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	room_name: Option<&DisplayName>,
	room_avatar: Option<&MxcUri>,
) -> (Option<Heroes>, Option<DisplayName>, Option<OwnedMxcUri>) {
	services
		.config
		.calculate_heroes
		.then_async(|| calculate_heroes(services, sender_user, room_id, room_name, room_avatar))
		.await
		.unwrap_or_default()
}

fn room_meta_future<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	room_id: &'a RoomId,
) -> impl Future<Output = (Option<DisplayName>, Option<OwnedMxcUri>, Option<bool>)> + Send + 'a {
	let room_name = services
		.state_accessor
		.get_name(room_id)
		.map_ok(Into::into)
		.map(Result::ok);

	let room_avatar = services
		.state_accessor
		.get_avatar(room_id)
		.map_ok(|content| content.url)
		.ok()
		.map(Option::flatten);

	let is_dm = services
		.state_accessor
		.is_direct(room_id, sender_user)
		.map(|is_dm| is_dm.then_some(is_dm));

	join3(room_name, room_avatar, is_dm)
}

fn member_counts_future<'a>(
	services: &'a Services,
	room_id: &'a RoomId,
) -> impl Future<Output = (Option<UInt>, Option<UInt>)> + Send + 'a {
	let joined_count = services
		.state_cache
		.room_joined_count(room_id)
		.map_ok(TryInto::try_into)
		.map_ok(Result::ok)
		.map(FlatOk::flat_ok);

	let invited_count = services
		.state_cache
		.room_invited_count(room_id)
		.map_ok(TryInto::try_into)
		.map_ok(Result::ok)
		.map(FlatOk::flat_ok);

	join(joined_count, invited_count)
}

fn notification_counts_future<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	room_id: &'a RoomId,
) -> impl Future<Output = (Option<UInt>, Option<UInt>, Result<u64>, ThreadCounts)> + Send + 'a {
	let highlight_count = services
		.pusher
		.highlight_count(sender_user, room_id)
		.map(TryInto::try_into)
		.map(Result::ok);

	let notification_count = services
		.pusher
		.notification_count(sender_user, room_id)
		.map(TryInto::try_into)
		.map(Result::ok);

	let last_read_count = services
		.pusher
		.last_notification_read(sender_user, room_id);

	let thread_counts = services
		.pusher
		.thread_notification_counts(sender_user, room_id);

	join4(highlight_count, notification_count, last_read_count, thread_counts)
}

// MSC3771/MSC3773: SSS v5 has no per-thread bucket; fold into the room total.
fn merge_unread_notifications(
	highlight_count: Option<UInt>,
	notification_count: Option<UInt>,
	thread_counts: &ThreadCounts,
) -> UnreadNotificationsCount {
	let (thread_notifications, thread_highlights) = thread_counts
		.values()
		.fold((0_u64, 0_u64), |(n, h), &(notifs, hl)| {
			(n.saturating_add(notifs), h.saturating_add(hl))
		});

	let merge = |total: u64| {
		move |count: UInt| count.saturating_add(UInt::try_from(total).unwrap_or_default())
	};

	UnreadNotificationsCount {
		highlight_count: highlight_count.map(merge(thread_highlights)),
		notification_count: notification_count.map(merge(thread_notifications)),
	}
}

async fn collect_required_state(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	required_state: &[(StateEventType, StateKey)],
	timeline_pdus: &[(PduCount, PduEvent)],
	encrypted: bool,
) -> Vec<Raw<AnySyncStateEvent>> {
	let lazy = required_state
		.iter()
		.any(is_equal_to!(&(StateEventType::RoomMember, "$LAZY".into())));

	let timeline_senders = timeline_pdus
		.iter()
		.filter(|_| lazy)
		.map(ref_at!(1))
		.map(Event::sender)
		.map(UserId::as_str);

	let timeline_member_targets = timeline_pdus
		.iter()
		.filter(|_| lazy)
		.map(ref_at!(1))
		.filter(|event| *event.event_type() == TimelineEventType::RoomMember)
		.filter_map(Event::state_key);

	let timeline_senders = timeline_senders
		.chain(timeline_member_targets)
		.sorted_unstable()
		.dedup()
		.map(|sender| (StateEventType::RoomMember, StateKey::from_str(sender)))
		.collect::<Vec<_>>();

	let wildcard_types: Vec<StateEventType> = required_state
		.iter()
		.filter(|(_, state_key)| state_key == "*")
		.map(|(event_type, _)| event_type.clone())
		.collect();

	let wildcard_state: Vec<(StateEventType, StateKey)> = wildcard_types
		.into_iter()
		.stream()
		.broad_then(|event_type| wildcard_state_keys(services, room_id, event_type))
		.concat()
		.await;

	let in_timeline = |event: &PduEvent| {
		timeline_pdus
			.iter()
			.map(ref_at!(1))
			.map(Event::event_id)
			.any(is_equal_to!(event.event_id()))
	};

	required_state
		.iter()
		.cloned()
		.stream()
		.chain(wildcard_state.into_iter().stream())
		.chain(timeline_senders.into_iter().stream())
		.broad_filter_map(async |state| {
			let state_key: StateKey = match state.1.as_str() {
				| "$LAZY" | "*" => return None,
				| "$ME" => sender_user.as_str().into(),
				| _ => state.1.clone(),
			};

			let mut pdu = services
				.state_accessor
				.room_state_get(room_id, &state.0, &state_key)
				.map_ok(Event::into_pdu)
				.ok()
				.await?;

			annotate_membership(services, &mut pdu, sender_user, encrypted).await;

			let pdu = strip_prev_state(pdu, sender_user, in_timeline);

			Some(Event::into_format(pdu))
		})
		.collect()
		.await
}

async fn wildcard_state_keys(
	services: &Services,
	room_id: &RoomId,
	event_type: StateEventType,
) -> Vec<(StateEventType, StateKey)> {
	services
		.state_accessor
		.room_state_keys(room_id, &event_type)
		.ready_filter_map(Result::ok)
		.map(|state_key| (event_type.clone(), state_key))
		.collect()
		.await
}
