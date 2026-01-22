use std::{
	cmp::Ordering,
	collections::{BTreeMap, HashSet},
};

use futures::{
	FutureExt, StreamExt, TryFutureExt, TryStreamExt,
	future::{join, join3, join4},
};
use ruma::{
	JsOption, MxcUri, OwnedMxcUri, OwnedRoomId, RoomId, UserId,
	api::client::sync::sync_events::{
		UnreadNotificationsCount,
		v5::{DisplayName, response, response::Heroes},
	},
	events::{
		StateEventType,
		TimelineEventType::{
			self, Beacon, CallInvite, PollStart, RoomEncrypted, RoomMessage, Sticker,
		},
		room::member::MembershipState,
	},
};
use tuwunel_core::{
	Result, at, err, error, is_equal_to,
	matrix::{Event, StateKey, pdu::PduCount},
	ref_at,
	utils::{
		BoolExt, IterStream, ReadyExt, TryFutureExtExt, math::usize_from_ruma, result::FlatOk,
		stream::BroadbandExt,
	},
};
use tuwunel_service::{Services, sync::Room};

use super::{super::load_timeline, Connection, SyncInfo, Window, WindowRoom};
use crate::client::ignored_filter;

static DEFAULT_BUMP_TYPES: [TimelineEventType; 6] =
	[CallInvite, PollStart, Beacon, RoomEncrypted, RoomMessage, Sticker];

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
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	WindowRoom {
		lists, membership, room_id, last_count, ..
	}: &WindowRoom,
) -> Result<response::Room> {
	debug_assert!(
		DEFAULT_BUMP_TYPES.is_sorted(),
		"DEFAULT_BUMP_TYPES must be sorted for binary search"
	);

	let &Room { roomsince, .. } = conn
		.rooms
		.get(room_id)
		.ok_or_else(|| err!("Missing connection state for {room_id}"))?;

	debug_assert!(
		*last_count > roomsince || *last_count == 0 || roomsince == 0,
		"Stale room shouldn't be in the window"
	);

	if *membership == Some(MembershipState::Leave) {
		return Ok(response::Room {
			initial: roomsince.eq(&0).then_some(true),
			lists: lists.clone(),
			membership: membership.clone(),
			prev_batch: Some(conn.next_batch.to_string().into()),
			limited: true,
			required_state: vec![
				services
					.state_accessor
					.room_state_get(room_id, &StateEventType::RoomMember, sender_user.as_str())
					.map_ok(Event::into_format)
					.await?,
			],

			..Default::default()
		});
	}

	let is_invite = *membership == Some(MembershipState::Invite);
	let default_details = (0_usize, HashSet::new());
	let (timeline_limit, required_state) = lists
		.iter()
		.filter_map(|list_id| conn.lists.get(list_id))
		.map(|list| &list.room_details)
		.chain(conn.subscriptions.get(room_id).into_iter())
		.fold(default_details, |(mut timeline_limit, mut required_state), config| {
			let limit = usize_from_ruma(config.timeline_limit);

			timeline_limit = timeline_limit.max(limit);
			required_state.extend(config.required_state.clone());

			(timeline_limit, required_state)
		});

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

	let (timeline_pdus, limited, _lastcount) = timeline
		.await
		.flat_ok()
		.unwrap_or_else(|| (Vec::new(), true, PduCount::default()));

	let required_state = required_state
		.into_iter()
		.filter(|_| !timeline_pdus.is_empty())
		.collect::<Vec<_>>();

	let prev_batch = timeline_pdus
		.first()
		.map(at!(0))
		.map(PduCount::into_unsigned)
		.as_ref()
		.map(ToString::to_string);

	let bump_stamp = timeline_pdus
		.iter()
		.filter(|(_, pdu)| {
			if *pdu.event_type() == TimelineEventType::RoomMember {
				return pdu
					.state_key()
					.is_some_and(is_equal_to!(sender_user.as_str()));
			}

			DEFAULT_BUMP_TYPES
				.binary_search(pdu.event_type())
				.is_ok()
		})
		.filter(|(_, pdu)| !pdu.is_redacted())
		.map(at!(0))
		.map(PduCount::into_signed)
		.max()
		.map(TryInto::try_into)
		.flat_ok();

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

	let lazy = required_state
		.iter()
		.any(is_equal_to!(&(StateEventType::RoomMember, "$LAZY".into())));

	let mut timeline_senders: Vec<_> = timeline_pdus
		.iter()
		.filter(|_| lazy)
		.map(ref_at!(1))
		.map(Event::sender)
		.collect();

	timeline_senders.sort();
	timeline_senders.dedup();
	let timeline_senders = timeline_senders
		.iter()
		.map(|sender| (StateEventType::RoomMember, StateKey::from_str(sender.as_str())))
		.stream();

	let wildcard_state = required_state
		.iter()
		.filter(|(_, state_key)| state_key == "*")
		.map(|(event_type, _)| {
			services
				.state_accessor
				.room_state_keys(room_id, event_type)
				.map_ok(|state_key| (event_type.clone(), state_key))
				.ready_filter_map(Result::ok)
		})
		.stream()
		.flatten();

	let required_state = required_state
		.iter()
		.cloned()
		.stream()
		.chain(wildcard_state)
		.chain(timeline_senders)
		.broad_filter_map(async |state| {
			let state_key: StateKey = match state.1.as_str() {
				| "$LAZY" | "*" => return None,
				| "$ME" => sender_user.as_str().into(),
				| _ => state.1.clone(),
			};

			services
				.state_accessor
				.room_state_get(room_id, &state.0, &state_key)
				.map_ok(Event::into_format)
				.ok()
				.await
		})
		.collect();

	// TODO: figure out a timestamp we can use for remote invites
	let invite_state = is_invite.then_async(|| {
		services
			.state_cache
			.invite_state(sender_user, room_id)
			.ok()
	});

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

	let is_dm = services
		.state_accessor
		.is_direct(room_id, sender_user)
		.map(|is_dm| is_dm.then_some(is_dm));

	let last_read_count = services
		.pusher
		.last_notification_read(sender_user, room_id);

	let timeline = timeline_pdus
		.iter()
		.stream()
		.filter_map(|item| ignored_filter(services, item.clone(), sender_user))
		.map(at!(1))
		.map(Event::into_format)
		.collect();

	let meta = join3(room_name, room_avatar, is_dm);
	let events = join4(timeline, num_live, required_state, invite_state);
	let member_counts = join(joined_count, invited_count);
	let notification_counts = join3(highlight_count, notification_count, last_read_count);
	let (
		(room_name, room_avatar, is_dm),
		(timeline, num_live, required_state, invite_state),
		(joined_count, invited_count),
		(highlight_count, notification_count, _last_notification_read),
	) = join4(meta, events, member_counts, notification_counts)
		.boxed()
		.await;

	let heroes = services
		.config
		.calculate_heroes
		.then_async(|| {
			calculate_heroes(
				services,
				sender_user,
				room_id,
				room_name.as_ref(),
				room_avatar.as_deref(),
			)
		})
		.await
		.unwrap_or_default();

	let (heroes, heroes_name, heroes_avatar) = heroes;

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
		unread_notifications: UnreadNotificationsCount { highlight_count, notification_count },
	})
}

#[tracing::instrument(name = "heroes", level = "trace", skip_all)]
async fn calculate_heroes(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	room_name: Option<&DisplayName>,
	room_avatar: Option<&MxcUri>,
) -> (Option<Heroes>, Option<DisplayName>, Option<OwnedMxcUri>) {
	const MAX_HEROES: usize = 5;

	let heroes: Heroes = services
		.state_cache
		.room_members(room_id)
		.ready_filter(|&member| member != sender_user)
		.ready_filter_map(|member| room_name.is_none().then_some(member))
		.map(ToOwned::to_owned)
		.broadn_filter_map(MAX_HEROES, async |user_id| {
			let content = services
				.state_accessor
				.get_member(room_id, &user_id)
				.await
				.ok()?;

			let name = content
				.displayname
				.is_none()
				.then_async(|| services.users.displayname(&user_id).ok());

			let avatar = content
				.avatar_url
				.is_none()
				.then_async(|| services.users.avatar_url(&user_id).ok());

			let (name, avatar) = join(name, avatar).await;
			let hero = response::Hero {
				user_id,
				avatar: avatar.unwrap_or(content.avatar_url),
				name: name
					.unwrap_or(content.displayname)
					.map(Into::into),
			};

			Some(hero)
		})
		.take(MAX_HEROES)
		.collect()
		.await;

	let hero_name = match heroes.len().cmp(&(1_usize)) {
		| Ordering::Less => None,
		| Ordering::Equal => Some(
			heroes[0]
				.name
				.clone()
				.unwrap_or_else(|| heroes[0].user_id.as_str().into()),
		),
		| Ordering::Greater => {
			let firsts = heroes[1..]
				.iter()
				.map(|h| {
					h.name
						.clone()
						.unwrap_or_else(|| h.user_id.as_str().into())
				})
				.collect::<Vec<_>>()
				.join(", ");

			let last = heroes[0]
				.name
				.clone()
				.unwrap_or_else(|| heroes[0].user_id.as_str().into());

			Some(format!("{firsts} and {last}")).map(Into::into)
		},
	};

	let heroes_avatar = (room_avatar.is_none() && room_name.is_none())
		.then(|| {
			heroes
				.first()
				.and_then(|hero| hero.avatar.clone())
		})
		.flatten();

	(Some(heroes), hero_name, heroes_avatar)
}
