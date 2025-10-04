use std::{
	cmp::Ordering,
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	mem::take,
	ops::Deref,
	time::Duration,
};

use axum::extract::State;
use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt,
	future::{OptionFuture, join, join3, join4, join5, try_join},
	pin_mut,
};
use ruma::{
	DeviceId, JsOption, MxcUri, OwnedEventId, OwnedMxcUri, OwnedRoomId, RoomId, UInt, UserId,
	api::client::sync::sync_events::{
		DeviceLists, UnreadNotificationsCount,
		v5::{Request, Response, request::ExtensionRoomConfig, response},
	},
	directory::RoomTypeFilter,
	events::{
		AnyRawAccountDataEvent, AnySyncEphemeralRoomEvent, StateEventType, TimelineEventType,
		receipt::SyncReceiptEvent,
		room::member::{MembershipState, RoomMemberEventContent},
		typing::TypingEventContent,
	},
	serde::Raw,
	uint,
};
use tokio::time::{Instant, timeout_at};
use tuwunel_core::{
	Err, Result, apply, at, debug_error, error, extract_variant, is_equal_to,
	matrix::{Event, StateKey, TypeStateKey, pdu::PduCount},
	ref_at, trace,
	utils::{
		BoolExt, FutureBoolExt, IterStream, ReadyExt, TryFutureExtExt,
		future::ReadyEqExt,
		math::{ruma_from_usize, usize_from_ruma},
		result::FlatOk,
		stream::{BroadbandExt, TryBroadbandExt, TryReadyExt, WidebandExt},
	},
	warn,
};
use tuwunel_service::{
	Services,
	rooms::read_receipt::pack_receipts,
	sync::{KnownRooms, into_snake_key},
};

use super::share_encrypted_room;
use crate::{
	Ruma,
	client::{DEFAULT_BUMP_TYPES, ignored_filter, sync::load_timeline},
};

type SyncInfo<'a> = (&'a UserId, &'a DeviceId, u64, &'a Request);
type TodoRooms = BTreeMap<OwnedRoomId, TodoRoom>;
type TodoRoom = (BTreeSet<TypeStateKey>, usize, u64);
type ResponseLists = BTreeMap<String, response::List>;

/// `POST /_matrix/client/unstable/org.matrix.simplified_msc3575/sync`
/// ([MSC4186])
///
/// A simplified version of sliding sync ([MSC3575]).
///
/// Get all new events in a sliding window of rooms since the last sync or a
/// given point in time.
///
/// [MSC3575]: https://github.com/matrix-org/matrix-spec-proposals/pull/3575
/// [MSC4186]: https://github.com/matrix-org/matrix-spec-proposals/pull/4186
#[tracing::instrument(
	name = "sync",
	level = "debug",
	skip_all,
	fields(
		user_id = %body.sender_user(),
		device_id = %body.sender_device(),
	)
)]
pub(crate) async fn sync_events_v5_route(
	State(ref services): State<crate::State>,
	mut body: Ruma<Request>,
) -> Result<Response> {
	debug_assert!(DEFAULT_BUMP_TYPES.is_sorted(), "DEFAULT_BUMP_TYPES is not sorted");

	let mut request = take(&mut body.body);
	let mut globalsince = request
		.pos
		.as_ref()
		.and_then(|string| string.parse().ok())
		.unwrap_or(0);

	let (sender_user, sender_device) = body.sender();
	let snake_key = into_snake_key(sender_user, sender_device, request.conn_id.as_deref());
	if globalsince != 0 && !services.sync.snake_connection_cached(&snake_key) {
		return Err!(Request(UnknownPos(
			"Connection data unknown to server; restarting sync stream."
		)));
	}

	// Client / User requested an initial sync
	if globalsince == 0 {
		services
			.sync
			.forget_snake_sync_connection(&snake_key);
	}

	// Get sticky parameters from cache
	let known_rooms = services
		.sync
		.update_snake_sync_request_with_cache(&snake_key, &mut request);

	let all_joined_rooms = services
		.state_cache
		.rooms_joined(sender_user)
		.map(ToOwned::to_owned)
		.collect::<Vec<OwnedRoomId>>();

	let all_invited_rooms = services
		.state_cache
		.rooms_invited(sender_user)
		.map(ToOwned::to_owned)
		.collect::<Vec<OwnedRoomId>>();

	let all_knocked_rooms = services
		.state_cache
		.rooms_knocked(sender_user)
		.map(ToOwned::to_owned)
		.collect::<Vec<OwnedRoomId>>();

	let (all_joined_rooms, all_invited_rooms, all_knocked_rooms) =
		join3(all_joined_rooms, all_invited_rooms, all_knocked_rooms).await;

	let all_invited_rooms = all_invited_rooms.iter().map(AsRef::as_ref);
	let all_knocked_rooms = all_knocked_rooms.iter().map(AsRef::as_ref);
	let all_joined_rooms = all_joined_rooms.iter().map(AsRef::as_ref);
	let all_rooms = all_joined_rooms
		.clone()
		.chain(all_invited_rooms.clone())
		.chain(all_knocked_rooms.clone());

	let sync_info: SyncInfo<'_> = (sender_user, sender_device, globalsince, &request);
	let (known_rooms, todo_rooms, lists) = handle_lists(
		services,
		sync_info,
		known_rooms,
		all_invited_rooms.clone(),
		all_joined_rooms.clone(),
		all_rooms.clone(),
	)
	.await;

	let timeout = request
		.timeout
		.as_ref()
		.map(Duration::as_millis)
		.map(TryInto::try_into)
		.flat_ok()
		.unwrap_or(services.config.client_sync_timeout_default)
		.max(services.config.client_sync_timeout_min)
		.min(services.config.client_sync_timeout_max);

	let stop_at = Instant::now()
		.checked_add(Duration::from_millis(timeout))
		.expect("configuration must limit maximum timeout");

	let mut response = Response {
		txn_id: request.txn_id.clone(),
		lists,
		pos: String::new(),
		rooms: Default::default(),
		extensions: Default::default(),
	};

	loop {
		let watchers = services.sync.watch(sender_user, sender_device);
		let next_batch = services.globals.wait_pending().await?;

		debug_assert!(globalsince <= next_batch, "next_batch is monotonic");
		if globalsince < next_batch {
			let rooms = handle_rooms(
				services,
				&sync_info,
				next_batch,
				&known_rooms,
				&todo_rooms,
				all_invited_rooms.clone(),
			)
			.map_ok(|rooms| response.rooms = rooms);

			let extensions = handle_extensions(
				services,
				sync_info,
				next_batch,
				&known_rooms,
				&todo_rooms,
				all_joined_rooms.clone(),
			)
			.map_ok(|extensions| response.extensions = extensions);

			try_join(rooms, extensions).boxed().await?;

			if !is_empty_response(&response) {
				trace!(globalsince, next_batch, "response {response:?}");
				response.pos = next_batch.to_string();
				return Ok(response);
			}
		}

		if timeout_at(stop_at, watchers).await.is_err() {
			trace!(globalsince, next_batch, "timeout; empty response");
			response.pos = next_batch.to_string();
			return Ok(response);
		}

		trace!(
			globalsince,
			last_batch = ?next_batch,
			count = ?services.globals.pending_count(),
			stop_at = ?stop_at,
			"notified by watcher"
		);

		globalsince = next_batch;
	}
}

fn is_empty_response(response: &Response) -> bool {
	response.extensions.is_empty()
		&& response
			.rooms
			.iter()
			.all(|(_, room)| room.timeline.is_empty() && room.invite_state.is_none())
}

#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(
        all_invited_rooms = all_invited_rooms.clone().count(),
        all_joined_rooms = all_joined_rooms.clone().count(),
        all_rooms = all_rooms.clone().count(),
        known_rooms = known_rooms.len(),
    )
)]
#[allow(clippy::too_many_arguments)]
async fn handle_lists<'a, Rooms, AllRooms>(
	services: &Services,
	sync_info: SyncInfo<'_>,
	known_rooms: KnownRooms,
	all_invited_rooms: Rooms,
	all_joined_rooms: Rooms,
	all_rooms: AllRooms,
) -> (KnownRooms, TodoRooms, ResponseLists)
where
	Rooms: Iterator<Item = &'a RoomId> + Clone + Send + 'a,
	AllRooms: Iterator<Item = &'a RoomId> + Clone + Send + 'a,
{
	let &(sender_user, sender_device, globalsince, request) = &sync_info;

	let mut todo_rooms: TodoRooms = BTreeMap::new();
	let mut response_lists = ResponseLists::new();
	for (list_id, list) in &request.lists {
		let active_rooms: Vec<_> = match list.filters.as_ref().and_then(|f| f.is_invite) {
			| None => all_rooms.clone().collect(),
			| Some(true) => all_invited_rooms.clone().collect(),
			| Some(false) => all_joined_rooms.clone().collect(),
		};

		let active_rooms = match list.filters.as_ref().map(|f| &f.not_room_types) {
			| None => active_rooms,
			| Some(filter) if filter.is_empty() => active_rooms,
			| Some(value) =>
				filter_rooms(
					services,
					value,
					&true,
					active_rooms.iter().stream().map(Deref::deref),
				)
				.collect()
				.await,
		};

		let mut new_known_rooms: BTreeSet<OwnedRoomId> = BTreeSet::new();
		let ranges = list.ranges.clone();
		for mut range in ranges {
			range.0 = uint!(0);
			range.1 = range
				.1
				.clamp(range.0, UInt::try_from(active_rooms.len()).unwrap_or(UInt::MAX));

			let room_ids =
				active_rooms[usize_from_ruma(range.0)..usize_from_ruma(range.1)].to_vec();

			let new_rooms: BTreeSet<OwnedRoomId> = room_ids
				.clone()
				.into_iter()
				.map(From::from)
				.collect();

			new_known_rooms.extend(new_rooms);
			for room_id in room_ids {
				let todo_room = todo_rooms.entry(room_id.to_owned()).or_insert((
					BTreeSet::new(),
					0_usize,
					u64::MAX,
				));

				todo_room.0.extend(
					list.room_details
						.required_state
						.iter()
						.map(|(ty, sk)| (ty.clone(), sk.as_str().into())),
				);

				let limit: usize = usize_from_ruma(list.room_details.timeline_limit).min(100);
				todo_room.1 = todo_room.1.max(limit);

				// 0 means unknown because it got out of date
				todo_room.2 = todo_room.2.min(
					known_rooms
						.get(list_id.as_str())
						.and_then(|k| k.get(room_id))
						.copied()
						.unwrap_or(0),
				);
			}
		}

		if let Some(conn_id) = request.conn_id.as_deref() {
			let snake_key = into_snake_key(sender_user, sender_device, conn_id.into());
			let list_id = list_id.as_str().into();
			services.sync.update_snake_sync_known_rooms(
				&snake_key,
				list_id,
				new_known_rooms,
				globalsince,
			);
		}

		response_lists.insert(list_id.clone(), response::List {
			count: ruma_from_usize(active_rooms.len()),
		});
	}

	let (known_rooms, todo_rooms) =
		fetch_subscriptions(services, sync_info, known_rooms, todo_rooms).await;

	(known_rooms, todo_rooms, response_lists)
}

#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		global_since,
		known_rooms = known_rooms.len(),
		todo_rooms = todo_rooms.len(),
	)
)]
async fn fetch_subscriptions(
	services: &Services,
	(sender_user, sender_device, globalsince, request): SyncInfo<'_>,
	known_rooms: KnownRooms,
	todo_rooms: TodoRooms,
) -> (KnownRooms, TodoRooms) {
	let subs = (todo_rooms, BTreeSet::new());
	let (todo_rooms, known_subs) = request
		.room_subscriptions
		.iter()
		.stream()
		.broad_filter_map(async |(room_id, room)| {
			let not_exists = services.metadata.exists(room_id).eq(&false);
			let is_disabled = services.metadata.is_disabled(room_id);
			let is_banned = services.metadata.is_banned(room_id);

			pin_mut!(not_exists, is_disabled, is_banned);
			not_exists
				.or(is_disabled)
				.or(is_banned)
				.await
				.eq(&false)
				.then_some((room_id, room))
		})
		.ready_fold(subs, |(mut todo_rooms, mut known_subs), (room_id, room)| {
			let todo_room =
				todo_rooms
					.entry(room_id.clone())
					.or_insert((BTreeSet::new(), 0_usize, u64::MAX));

			todo_room.0.extend(
				room.required_state
					.iter()
					.map(|(ty, sk)| (ty.clone(), sk.as_str().into())),
			);

			let limit: UInt = room.timeline_limit;
			todo_room.1 = todo_room.1.max(usize_from_ruma(limit));

			// 0 means unknown because it got out of date
			todo_room.2 = todo_room.2.min(
				known_rooms
					.get("subscriptions")
					.and_then(|k| k.get(room_id))
					.copied()
					.unwrap_or(0),
			);

			known_subs.insert(room_id.clone());
			(todo_rooms, known_subs)
		})
		.await;

	if let Some(conn_id) = request.conn_id.as_deref() {
		let snake_key = into_snake_key(sender_user, sender_device, conn_id.into());
		let list_id = "subscriptions".into();
		services
			.sync
			.update_snake_sync_known_rooms(&snake_key, list_id, known_subs, globalsince);
	}

	(known_rooms, todo_rooms)
}

#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(?filters, negate)
)]
fn filter_rooms<'a, Rooms>(
	services: &'a Services,
	filters: &'a [RoomTypeFilter],
	negate: &'a bool,
	rooms: Rooms,
) -> impl Stream<Item = &'a RoomId> + Send + 'a
where
	Rooms: Stream<Item = &'a RoomId> + Send + 'a,
{
	rooms
		.wide_filter_map(async |room_id| {
			match services
				.state_accessor
				.get_room_type(room_id)
				.await
			{
				| Ok(room_type) => Some((room_id, Some(room_type))),
				| Err(e) if e.is_not_found() => Some((room_id, None)),
				| Err(_) => None,
			}
		})
		.map(|(room_id, room_type)| (room_id, RoomTypeFilter::from(room_type)))
		.ready_filter_map(|(room_id, room_type_filter)| {
			let contains = filters.contains(&room_type_filter);
			let pos = !*negate && (filters.is_empty() || contains);
			let neg = *negate && !contains;

			(pos || neg).then_some(room_id)
		})
}

#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(
        next_batch,
        all_invited_rooms = all_invited_rooms.clone().count(),
        todo_rooms = todo_rooms.len(),
    )
)]
async fn handle_rooms<'a, Rooms>(
	services: &Services,
	sync_info: &SyncInfo<'_>,
	next_batch: u64,
	_known_rooms: &KnownRooms,
	todo_rooms: &TodoRooms,
	all_invited_rooms: Rooms,
) -> Result<BTreeMap<OwnedRoomId, response::Room>>
where
	Rooms: Iterator<Item = &'a RoomId> + Clone + Send + Sync + 'a,
{
	let rooms: BTreeMap<_, _> = todo_rooms
		.iter()
		.try_stream()
		.broad_and_then(async |(room_id, todo_room)| {
			let is_invited = all_invited_rooms
				.clone()
				.any(is_equal_to!(room_id));

			let room =
				handle_room(services, next_batch, sync_info, room_id, todo_room, is_invited)
					.await?;

			Ok((room_id, room))
		})
		.ready_try_filter_map(|(room_id, room)| Ok(room.map(|room| (room_id, room))))
		.map_ok(|(room_id, room)| (room_id.to_owned(), room))
		.try_collect()
		.await?;

	Ok(rooms)
}

#[tracing::instrument(level = "debug", skip_all, fields(room_id, roomsince))]
#[allow(clippy::too_many_arguments)]
async fn handle_room(
	services: &Services,
	next_batch: u64,
	(sender_user, _, _globalsince, _): &SyncInfo<'_>,
	room_id: &RoomId,
	(required_state_request, timeline_limit, roomsince): &TodoRoom,
	is_invited: bool,
) -> Result<Option<response::Room>> {
	let timeline: OptionFuture<_> = is_invited
		.eq(&false)
		.then(|| {
			load_timeline(
				services,
				sender_user,
				room_id,
				PduCount::Normal(*roomsince),
				Some(PduCount::from(next_batch)),
				*timeline_limit,
			)
		})
		.into();

	let Ok(timeline) = timeline.await.transpose() else {
		debug_error!(?room_id, "Missing timeline.");
		return Ok(None);
	};

	let (timeline_pdus, limited, _lastcount) =
		timeline.unwrap_or_else(|| (Vec::new(), true, PduCount::default()));

	if *roomsince != 0 && timeline_pdus.is_empty() && !is_invited {
		return Ok(None);
	}

	let prev_batch = timeline_pdus
		.first()
		.map(at!(0))
		.map(PduCount::into_unsigned)
		.or_else(|| roomsince.ne(&0).then_some(*roomsince))
		.as_ref()
		.map(ToString::to_string);

	let bump_stamp = timeline_pdus
		.iter()
		.filter(|(_, pdu)| {
			DEFAULT_BUMP_TYPES
				.binary_search(pdu.event_type())
				.is_ok()
		})
		.fold(Option::<UInt>::None, |mut bump_stamp, (_, pdu)| {
			let ts = pdu.origin_server_ts().get();
			if bump_stamp.is_none_or(|bump_stamp| bump_stamp < ts) {
				bump_stamp.replace(ts);
			}

			bump_stamp
		});

	let lazy = required_state_request
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
		.map(|sender| (StateEventType::RoomMember, StateKey::from_str(sender.as_str())));

	let required_state = required_state_request
		.iter()
		.cloned()
		.chain(timeline_senders)
		.stream()
		.broad_filter_map(async |state| {
			let state_key: StateKey = match state.1.as_str() {
				| "$LAZY" => return None,
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
	let invite_state: OptionFuture<_> = is_invited
		.then(|| {
			services
				.state_cache
				.invite_state(sender_user, room_id)
				.ok()
		})
		.into();

	let timeline = timeline_pdus
		.iter()
		.stream()
		.filter_map(|item| ignored_filter(services, item.clone(), sender_user))
		.map(at!(1))
		.map(Event::into_format)
		.collect();

	let room_name = services
		.state_accessor
		.get_name(room_id)
		.map(Result::ok);

	let room_avatar = services
		.state_accessor
		.get_avatar(room_id)
		.map_ok(|content| content.url)
		.ok()
		.map(Option::flatten);

	let highlight_count = services
		.user
		.highlight_count(sender_user, room_id)
		.map(TryInto::try_into)
		.map(Result::ok);

	let notification_count = services
		.user
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

	let meta = join(room_name, room_avatar);
	let events = join3(timeline, required_state, invite_state);
	let member_counts = join(joined_count, invited_count);
	let notification_counts = join(highlight_count, notification_count);
	let (
		(room_name, room_avatar),
		(timeline, required_state, invite_state),
		(joined_count, invited_count),
		(highlight_count, notification_count),
	) = join4(meta, events, member_counts, notification_counts)
		.boxed()
		.await;

	let (heroes, hero_name, heroes_avatar) = calculate_heroes(
		services,
		sender_user,
		room_id,
		room_name.as_deref(),
		room_avatar.as_deref(),
	)
	.await?;

	let num_live = None; // Count events in timeline greater than global sync counter

	Ok(Some(response::Room {
		initial: Some(*roomsince == 0),
		name: room_name.or(hero_name),
		avatar: JsOption::from_option(room_avatar.or(heroes_avatar)),
		invite_state: invite_state.flatten(),
		required_state,
		timeline,
		is_dm: None,
		prev_batch,
		limited,
		bump_stamp,
		heroes,
		num_live,
		joined_count,
		invited_count,
		unread_notifications: UnreadNotificationsCount { highlight_count, notification_count },
	}))
}

#[tracing::instrument(level = "debug", skip_all, fields(room_id, roomsince))]
#[allow(clippy::type_complexity)]
async fn calculate_heroes(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	room_name: Option<&str>,
	room_avatar: Option<&MxcUri>,
) -> Result<(Option<Vec<response::Hero>>, Option<String>, Option<OwnedMxcUri>)> {
	const MAX_HEROES: usize = 5;
	let heroes: Vec<_> = services
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

			let name: OptionFuture<_> = content
				.displayname
				.is_none()
				.then(|| services.users.displayname(&user_id).ok())
				.into();

			let avatar: OptionFuture<_> = content
				.avatar_url
				.is_none()
				.then(|| services.users.avatar_url(&user_id).ok())
				.into();

			let (name, avatar) = join(name, avatar).await;
			let hero = response::Hero {
				user_id,
				name: name.unwrap_or(content.displayname),
				avatar: avatar.unwrap_or(content.avatar_url),
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
				.unwrap_or_else(|| heroes[0].user_id.to_string()),
		),
		| Ordering::Greater => {
			let firsts = heroes[1..]
				.iter()
				.map(|h| {
					h.name
						.clone()
						.unwrap_or_else(|| h.user_id.to_string())
				})
				.collect::<Vec<_>>()
				.join(", ");

			let last = heroes[0]
				.name
				.clone()
				.unwrap_or_else(|| heroes[0].user_id.to_string());

			Some(format!("{firsts} and {last}"))
		},
	};

	let heroes_avatar = (room_avatar.is_none() && room_name.is_none())
		.then(|| {
			heroes
				.first()
				.and_then(|hero| hero.avatar.clone())
		})
		.flatten();

	Ok((Some(heroes), hero_name, heroes_avatar))
}

#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		global_since,
		known_rooms = known_rooms.len(),
	)
)]
async fn handle_extensions<'a, Rooms>(
	services: &Services,
	sync_info: SyncInfo<'_>,
	next_batch: u64,
	known_rooms: &KnownRooms,
	todo_rooms: &TodoRooms,
	all_joined_rooms: Rooms,
) -> Result<response::Extensions>
where
	Rooms: Iterator<Item = &'a RoomId> + Clone + Send + 'a,
{
	let &(_, _, _, request) = &sync_info;

	let account_data: OptionFuture<_> = request
		.extensions
		.account_data
		.enabled
		.unwrap_or(false)
		.then(|| collect_account_data(services, sync_info, next_batch, known_rooms, todo_rooms))
		.into();

	let receipts: OptionFuture<_> = request
		.extensions
		.receipts
		.enabled
		.unwrap_or(false)
		.then(|| collect_receipts(services, sync_info, next_batch, known_rooms, todo_rooms))
		.into();

	let typing: OptionFuture<_> = request
		.extensions
		.typing
		.enabled
		.unwrap_or(false)
		.then(|| collect_typing(services, sync_info, next_batch, known_rooms, todo_rooms))
		.into();

	let to_device: OptionFuture<_> = request
		.extensions
		.to_device
		.enabled
		.unwrap_or(false)
		.then(|| collect_to_device(services, sync_info, next_batch))
		.into();

	let e2ee: OptionFuture<_> = request
		.extensions
		.e2ee
		.enabled
		.unwrap_or(false)
		.then(|| {
			collect_e2ee(services, sync_info, next_batch, todo_rooms, all_joined_rooms.clone())
		})
		.into();

	let (account_data, receipts, typing, to_device, e2ee) =
		join5(account_data, receipts, typing, to_device, e2ee)
			.map(apply!(5, |t: Option<_>| t.unwrap_or(Ok(Default::default()))))
			.await;

	Ok(response::Extensions {
		account_data: account_data?,
		receipts: receipts?,
		typing: typing?,
		to_device: to_device?,
		e2ee: e2ee?,
	})
}

#[tracing::instrument(level = "trace", skip_all, fields(globalsince, next_batch))]
async fn collect_account_data(
	services: &Services,
	sync_info: SyncInfo<'_>,
	next_batch: u64,
	known_rooms: &KnownRooms,
	todo_rooms: &TodoRooms,
) -> Result<response::AccountData> {
	let (sender_user, _, globalsince, request) = sync_info;
	let data = &request.extensions.account_data;
	let rooms = extension_rooms_todo(
		sync_info,
		known_rooms,
		todo_rooms,
		data.lists.as_ref(),
		data.rooms.as_ref(),
	)
	.stream()
	.broad_filter_map(async |room_id| {
		let &(_, _, roomsince) = todo_rooms.get(room_id)?;
		let changes: Vec<_> = services
			.account_data
			.changes_since(Some(room_id), sender_user, roomsince, Some(next_batch))
			.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Room))
			.collect()
			.await;

		changes
			.is_empty()
			.eq(&false)
			.then(move || (room_id.to_owned(), changes))
	})
	.collect();

	let global = services
		.account_data
		.changes_since(None, sender_user, globalsince, Some(next_batch))
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Global))
		.collect();

	let (global, rooms) = join(global, rooms).await;

	Ok(response::AccountData { global, rooms })
}

#[tracing::instrument(level = "trace", skip_all)]
async fn collect_receipts(
	services: &Services,
	sync_info: SyncInfo<'_>,
	next_batch: u64,
	known_rooms: &KnownRooms,
	todo_rooms: &TodoRooms,
) -> Result<response::Receipts> {
	let (_, _, _, request) = sync_info;
	let data = &request.extensions.receipts;
	let rooms = extension_rooms_todo(
		sync_info,
		known_rooms,
		todo_rooms,
		data.lists.as_ref(),
		data.rooms.as_ref(),
	)
	.stream()
	.broad_filter_map(async |room_id| {
		collect_receipt(services, sync_info, next_batch, todo_rooms, room_id).await
	})
	.collect()
	.await;

	Ok(response::Receipts { rooms })
}

async fn collect_receipt(
	services: &Services,
	(sender_user, ..): SyncInfo<'_>,
	next_batch: u64,
	todo_rooms: &TodoRooms,
	room_id: &RoomId,
) -> Option<(OwnedRoomId, Raw<SyncReceiptEvent>)> {
	let &(_, _, roomsince) = todo_rooms.get(room_id)?;
	let private_receipt = services
		.read_receipt
		.last_privateread_update(sender_user, room_id)
		.then(async |last_private_update| {
			if last_private_update <= roomsince || last_private_update > next_batch {
				return None;
			}

			services
				.read_receipt
				.private_read_get(room_id, sender_user)
				.map(Some)
				.await
		})
		.map(Option::into_iter)
		.map(Iterator::flatten)
		.map(IterStream::stream)
		.flatten_stream();

	let receipts: Vec<Raw<AnySyncEphemeralRoomEvent>> = services
		.read_receipt
		.readreceipts_since(room_id, roomsince, Some(next_batch))
		.filter_map(async |(read_user, _ts, v)| {
			services
				.users
				.user_is_ignored(read_user, sender_user)
				.await
				.or_some(v)
		})
		.chain(private_receipt)
		.collect()
		.boxed()
		.await;

	receipts
		.is_empty()
		.eq(&false)
		.then(|| (room_id.to_owned(), pack_receipts(receipts.into_iter())))
}

#[tracing::instrument(level = "trace", skip_all, fields(globalsince))]
async fn collect_typing(
	services: &Services,
	sync_info: SyncInfo<'_>,
	_next_batch: u64,
	known_rooms: &KnownRooms,
	todo_rooms: &TodoRooms,
) -> Result<response::Typing> {
	use response::Typing;
	use ruma::events::typing::SyncTypingEvent;

	let (sender_user, _, _, request) = sync_info;
	let data = &request.extensions.typing;
	extension_rooms_todo(
		sync_info,
		known_rooms,
		todo_rooms,
		data.lists.as_ref(),
		data.rooms.as_ref(),
	)
	.stream()
	.filter_map(async |room_id| {
		services
			.typing
			.typing_users_for_user(room_id, sender_user)
			.inspect_err(|e| debug_error!(%room_id, "Failed to get typing events: {e}"))
			.await
			.ok()
			.filter(|users| !users.is_empty())
			.map(|users| (room_id, users))
	})
	.ready_filter_map(|(room_id, users)| {
		let content = TypingEventContent::new(users);
		let event = SyncTypingEvent { content };
		let event = Raw::new(&event);

		Some((room_id.to_owned(), event.ok()?))
	})
	.collect::<BTreeMap<_, _>>()
	.map(|rooms| Typing { rooms })
	.map(Ok)
	.await
}

#[tracing::instrument(level = "trace", skip_all, fields(globalsince, next_batch))]
async fn collect_to_device(
	services: &Services,
	(sender_user, sender_device, globalsince, _request): SyncInfo<'_>,
	next_batch: u64,
) -> Result<Option<response::ToDevice>> {
	services
		.users
		.remove_to_device_events(sender_user, sender_device, globalsince)
		.await;

	let events: Vec<_> = services
		.users
		.get_to_device_events(sender_user, sender_device, None, Some(next_batch))
		.collect()
		.await;

	let to_device = events
		.is_empty()
		.eq(&false)
		.then(|| response::ToDevice {
			next_batch: next_batch.to_string(),
			events,
		});

	Ok(to_device)
}

// TODO ----------------------------------------------------------------------

#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(
		globalsince,
		next_batch,
		all_joined_rooms = all_joined_rooms.clone().count(),
	)
)]
async fn collect_e2ee<'a, Rooms>(
	services: &Services,
	(sender_user, sender_device, globalsince, _): SyncInfo<'_>,
	next_batch: u64,
	_todo_rooms: &TodoRooms,
	all_joined_rooms: Rooms,
) -> Result<response::E2EE>
where
	Rooms: Iterator<Item = &'a RoomId> + Clone + Send + 'a,
{
	// Users that have left any encrypted rooms the sender was in
	let mut left_encrypted_users = HashSet::new();
	let mut device_list_changes = HashSet::new();
	let mut device_list_left = HashSet::new();
	// Look for device list updates of this account
	device_list_changes.extend(
		services
			.users
			.keys_changed(sender_user, globalsince, Some(next_batch))
			.map(ToOwned::to_owned)
			.collect::<Vec<_>>()
			.await,
	);

	for room_id in all_joined_rooms {
		let Ok(current_shortstatehash) = services
			.state
			.get_room_shortstatehash(room_id)
			.await
		else {
			error!("Room {room_id} has no state");
			continue;
		};

		let since_shortstatehash = services
			.timeline
			.next_shortstatehash(room_id, PduCount::Normal(globalsince))
			.await
			.ok();

		let encrypted_room = services
			.state_accessor
			.state_get(current_shortstatehash, &StateEventType::RoomEncryption, "")
			.await
			.is_ok();

		if let Some(since_shortstatehash) = since_shortstatehash {
			// Skip if there are only timeline changes
			if since_shortstatehash == current_shortstatehash {
				continue;
			}

			let synced_shortstatehash = services
				.timeline
				.prev_shortstatehash(room_id, PduCount::Normal(globalsince).saturating_add(1))
				.await
				.ok();

			let synced_sender_member: OptionFuture<_> = synced_shortstatehash
				.map(|shortstatehash| {
					services
						.state_accessor
						.state_get_content(
							shortstatehash,
							&StateEventType::RoomMember,
							sender_user.as_str(),
						)
						.map_ok(|content: RoomMemberEventContent| content)
				})
				.into();

			let joined_since_last_sync = synced_sender_member
				.await
				.and_then(Result::ok)
				.as_ref()
				.is_none_or(|member| member.membership != MembershipState::Join);

			let synced_encryption: OptionFuture<_> = synced_shortstatehash
				.map(|shortstatehash| {
					services.state_accessor.state_get(
						shortstatehash,
						&StateEventType::RoomEncryption,
						"",
					)
				})
				.into();

			let synced_encryption = synced_encryption.await.and_then(Result::ok);

			let new_encrypted_room = encrypted_room && synced_encryption.is_none();

			if encrypted_room {
				let current_state_ids: HashMap<_, OwnedEventId> = services
					.state_accessor
					.state_full_ids(current_shortstatehash)
					.collect()
					.await;

				let since_state_ids: HashMap<_, _> = services
					.state_accessor
					.state_full_ids(since_shortstatehash)
					.collect()
					.await;

				for (key, id) in current_state_ids {
					if since_state_ids.get(&key) == Some(&id) {
						continue;
					}

					let Ok(pdu) = services.timeline.get_pdu(&id).await else {
						error!("Pdu in state not found: {id}");
						continue;
					};

					if pdu.kind != TimelineEventType::RoomMember {
						continue;
					}

					let Some(Ok(user_id)) = pdu.state_key.as_deref().map(UserId::parse) else {
						continue;
					};

					if user_id == sender_user {
						continue;
					}

					let content: RoomMemberEventContent = pdu.get_content()?;
					match content.membership {
						| MembershipState::Join => {
							// A new user joined an encrypted room
							if !share_encrypted_room(
								services,
								sender_user,
								user_id,
								Some(room_id),
							)
							.await
							{
								device_list_changes.insert(user_id.to_owned());
							}
						},
						| MembershipState::Leave => {
							// Write down users that have left encrypted rooms we
							// are in
							left_encrypted_users.insert(user_id.to_owned());
						},
						| _ => {},
					}
				}

				if joined_since_last_sync || new_encrypted_room {
					// If the user is in a new encrypted room, give them all joined users
					device_list_changes.extend(
						services
						.state_cache
						.room_members(room_id)
						// Don't send key updates from the sender to the sender
						.ready_filter(|user_id| sender_user != *user_id)
						// Only send keys if the sender doesn't share an encrypted room with the target
						// already
						.filter_map(|user_id| {
							share_encrypted_room(services, sender_user, user_id, Some(room_id))
								.map(|res| res.or_some(user_id.to_owned()))
						})
						.collect::<Vec<_>>()
						.await,
					);
				}
			}
		}
		// Look for device list updates in this room
		device_list_changes.extend(
			services
				.users
				.room_keys_changed(room_id, globalsince, Some(next_batch))
				.map(|(user_id, _)| user_id)
				.map(ToOwned::to_owned)
				.collect::<Vec<_>>()
				.await,
		);
	}

	for user_id in left_encrypted_users {
		let dont_share_encrypted_room =
			!share_encrypted_room(services, sender_user, &user_id, None).await;

		// If the user doesn't share an encrypted room with the target anymore, we need
		// to tell them
		if dont_share_encrypted_room {
			device_list_left.insert(user_id);
		}
	}

	let last_otk_update = services
		.users
		.last_one_time_keys_update(sender_user)
		.await;

	let device_otk_count: OptionFuture<_> = last_otk_update
		.gt(&globalsince)
		.then(|| {
			services
				.users
				.count_one_time_keys(sender_user, sender_device)
		})
		.into();

	Ok(response::E2EE {
		device_one_time_keys_count: device_otk_count.await.unwrap_or_default(),

		device_unused_fallback_key_types: None,

		device_lists: DeviceLists {
			changed: device_list_changes.into_iter().collect(),
			left: device_list_left.into_iter().collect(),
		},
	})
}

// ----------------------------------------------------------------------------

fn extension_rooms_todo<'a>(
	(_, _, _, request): SyncInfo<'a>,
	known_rooms: &'a KnownRooms,
	todo_rooms: &'a TodoRooms,
	lists: Option<&'a Vec<String>>,
	rooms: Option<&'a Vec<ExtensionRoomConfig>>,
) -> impl Iterator<Item = &'a RoomId> + Send + 'a {
	let lists_explicit = lists.into_iter().flat_map(|vec| vec.iter());

	let lists_requested = request
		.lists
		.keys()
		.filter(move |_| lists.is_none());

	let rooms_explicit = rooms
		.into_iter()
		.flat_map(|vec| vec.iter())
		.filter_map(|erc| extract_variant!(erc, ExtensionRoomConfig::Room))
		.map(AsRef::<RoomId>::as_ref);

	let rooms_implicit = todo_rooms
		.keys()
		.map(AsRef::as_ref)
		.filter(move |_| rooms.is_none());

	lists_explicit
		.chain(lists_requested)
		.flat_map(|list_id| {
			known_rooms
				.get(list_id.as_str())
				.into_iter()
				.flat_map(BTreeMap::keys)
		})
		.map(AsRef::as_ref)
		.chain(rooms_explicit)
		.chain(rooms_implicit)
}
