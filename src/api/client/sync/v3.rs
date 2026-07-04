use std::{
	collections::{BTreeMap, HashMap, HashSet},
	time::Duration,
};

use axum::extract::State;
use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, join3, join4, join5},
	pin_mut,
};
use ruma::{
	DeviceId, EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UInt, UserId,
	api::client::{
		filter::FilterDefinition,
		sync::sync_events::{
			self, DeviceLists, UnreadNotificationsCount,
			v3::{
				Ephemeral, Filter, GlobalAccountData, InviteState, InvitedRoom, JoinedRoom,
				KnockState, KnockedRoom, LeftRoom, Presence, RoomAccountData, RoomSummary, Rooms,
				State as RoomState, StateEvents, Timeline, ToDevice,
			},
		},
	},
	events::{
		AnyGlobalAccountDataEvent, AnyRawAccountDataEvent, AnyRoomAccountDataEvent,
		AnySyncEphemeralRoomEvent, AnySyncStateEvent, StateEventType, SyncEphemeralRoomEvent,
		TimelineEventType::*,
		presence::{PresenceEvent, PresenceEventContent},
		room::member::{MembershipState, RoomMemberEventContent},
		typing::TypingEventContent,
	},
	serde::Raw,
	uint,
};
use tokio::time;
use tuwunel_core::{
	Result, at,
	debug::INFO_SPAN_LEVEL,
	debug_error, err,
	error::{inspect_debug_log, inspect_log},
	extract_variant, is_equal_to, is_false, is_true,
	matrix::{
		Event,
		event::{Matches, trim_event_fields},
		pdu::{EventHash, PduCount, PduEvent},
	},
	pair_of, ref_at,
	result::FlatOk,
	smallvec::SmallVec,
	trace,
	utils::{
		self, BoolExt, FutureBoolExt, IterStream, ReadyExt, TryFutureExtExt,
		future::{OptionStream, ReadyBoolExt},
		math::ruma_from_u64,
		option::OptionExt,
		result::MapExpect,
		stream::{BroadbandExt, Tools, TryExpect, WidebandExt},
	},
};
use tuwunel_service::{
	Services,
	rooms::{
		lazy_loading,
		lazy_loading::{Options, Witness},
		read_receipt::PrivateReadEvents,
		short::{ShortEventId, ShortStateHash, ShortStateKey},
	},
};

use super::{load_timeline, share_encrypted_room, strip_prev_state};
use crate::{
	ClientIp, Ruma,
	client::{ignored_filter, is_empty_account_data_event, with_membership},
};

type TimelineEventIds = SmallVec<[OwnedEventId; 1]>;

/// MSC4222 `state_after` opt-in: which room-state field the response carries.
#[derive(Clone, Copy, Debug)]
enum StateAfter {
	Off,
	Stable,
	Unstable,
}

impl StateAfter {
	fn requested(self) -> bool { !matches!(self, Self::Off) }

	fn wrap(self, events: StateEvents) -> RoomState {
		match self {
			| Self::Off => RoomState::Before(events),
			| Self::Stable => RoomState::After(events),
			| Self::Unstable => RoomState::AfterUnstable(events),
		}
	}
}

impl From<(bool, bool)> for StateAfter {
	fn from((stable, unstable): (bool, bool)) -> Self {
		// Unstable opt-in wins: such a client reads the unstable field name.
		match (stable, unstable) {
			| (_, true) => Self::Unstable,
			| (true, _) => Self::Stable,
			| _ => Self::Off,
		}
	}
}

#[derive(Default)]
struct StateChanges {
	heroes: Option<Vec<OwnedUserId>>,
	joined_member_count: Option<u64>,
	invited_member_count: Option<u64>,
	state_events: Vec<PduEvent>,
}

type PresenceUpdates = HashMap<OwnedUserId, PresenceEventContent>;

struct RoomMetadata {
	since_shortstatehash: Option<ShortStateHash>,
	horizon_shortstatehash: Option<ShortStateHash>,
	after_shortstatehash: Option<ShortStateHash>,
	current_shortstatehash: Option<ShortStateHash>,
	receipt_events: Vec<(OwnedUserId, Raw<AnySyncEphemeralRoomEvent>)>,
	encrypted_room: Option<bool>,
}

struct UserMetadata {
	witness: Option<Witness>,
	#[expect(clippy::option_option)]
	last_notification_read: Option<Option<u64>>,
	thread_last_reads: Option<BTreeMap<OwnedEventId, u64>>,
	last_privateread_update: u64,
	joined_since_last_sync: bool,
}

struct NotificationGates<F> {
	send_notification_counts: bool,
	send_notification_count_filter: F,
}

struct BuildJoinedRoom {
	receipt_events: Vec<(OwnedUserId, Raw<AnySyncEphemeralRoomEvent>)>,
	typing_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
	private_read_events: Option<PrivateReadEvents>,
	state_events: Vec<Raw<AnySyncStateEvent>>,
	account_data_events: Vec<Raw<AnyRoomAccountDataEvent>>,
	room_events: Vec<PduEvent>,
	heroes: Option<Vec<OwnedUserId>>,
	joined_member_count: Option<u64>,
	invited_member_count: Option<u64>,
	unread_notifications: UnreadNotificationsCount,
	unread_thread_notifications: BTreeMap<OwnedEventId, UnreadNotificationsCount>,
	state_after: StateAfter,
	limited: bool,
	joined_since_last_sync: bool,
	prev_batch: Option<PduCount>,
}

/// # `GET /_matrix/client/r0/sync`
///
/// Synchronize the client's state with the latest state on the server.
///
/// - This endpoint takes a `since` parameter which should be the `next_batch`
///   value from a previous request for incremental syncs.
///
/// Calling this endpoint without a `since` parameter returns:
/// - Some of the most recent events of each timeline
/// - Notification counts for each room
/// - Joined and invited member counts, heroes
/// - All state events
///
/// Calling this endpoint with a `since` parameter from a previous `next_batch`
/// returns: For joined rooms:
/// - Some of the most recent events of each timeline that happened after since
/// - If user joined the room after since: All state events (unless lazy loading
///   is activated) and all device list updates in that room
/// - If the user was already in the room: A list of all events that are in the
///   state now, but were not in the state at `since`
/// - If the state we send contains a member event: Joined and invited member
///   counts, heroes
/// - Device list updates that happened after `since`
/// - If there are events in the timeline we send or the user send updated his
///   read mark: Notification counts
/// - EDUs that are active now (read receipts, typing updates, presence)
/// - TODO: Allow multiple sync streams to support Pantalaimon
///
/// For invited rooms:
/// - If the user was invited after `since`: A subset of the state of the room
///   at the point of the invite
///
/// For left rooms:
/// - If the user left after `since`: `prev_batch` token, empty state (TODO:
///   subset of the state at the point of the leave)
#[tracing::instrument(
	name = "sync",
	level = "debug",
	skip_all,
	fields(
		user_id = %body.sender_user(),
		device_id = %body.sender_device.as_deref().map_or("<no device>", |x| x.as_str()),
    )
)]
pub(crate) async fn sync_events_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<sync_events::v3::Request>,
) -> Result<sync_events::v3::Response> {
	let sender_user = body.sender_user();
	let sender_device = body.sender_device.as_deref();

	let filter = body
		.body
		.filter
		.as_ref()
		.map_async(async |filter| match filter {
			| Filter::FilterDefinition(filter) => filter.clone(),
			| Filter::FilterId(filter_id) => services
				.users
				.get_filter(sender_user, filter_id)
				.await
				.unwrap_or_default(),
		});

	let filter = filter.map(Option::unwrap_or_default);
	let full_state = body.body.full_state;
	let set_presence = &body.body.set_presence;
	let state_after =
		StateAfter::from((body.body.use_state_after, body.body.use_state_after_unstable));

	let ping_presence = services
		.presence
		.maybe_ping_presence(
			sender_user,
			body.sender_device.as_deref(),
			Some(client),
			set_presence,
		)
		.inspect_err(inspect_log)
		.ok();

	// Record user as actively syncing for push suppression heuristic.
	let note_sync = services.presence.note_sync(sender_user);

	let (filter, ..) = join3(filter, ping_presence, note_sync).await;

	let mut since = body
		.body
		.since
		.as_deref()
		.map(str::parse)
		.flat_ok()
		.unwrap_or(0);

	let timeout = body
		.body
		.timeout
		.as_ref()
		.map(Duration::as_millis)
		.map(TryInto::try_into)
		.flat_ok()
		.unwrap_or(services.config.client_sync_timeout_default)
		.max(services.config.client_sync_timeout_min)
		.min(services.config.client_sync_timeout_max);

	let stop_at = time::Instant::now()
		.checked_add(Duration::from_millis(timeout))
		.expect("configuration must limit maximum timeout");

	loop {
		let watch_rooms = services
			.state_cache
			.rooms_joined(sender_user)
			.chain(services.state_cache.rooms_invited(sender_user));

		let watchers = services
			.sync
			.watch(sender_user, sender_device, watch_rooms)
			.await;

		let next_batch = services.globals.wait_pending().await?;
		if since > next_batch {
			debug_error!(since, next_batch, "received since > next_batch, clamping");
			since = next_batch;
		}

		if since < next_batch || full_state {
			let response = build_sync_events(
				&services,
				sender_user,
				sender_device,
				since,
				next_batch,
				full_state,
				state_after,
				&filter,
			)
			.await?;

			let empty = response.rooms.is_empty()
				&& response.presence.is_empty()
				&& response.account_data.is_empty()
				&& response.device_lists.is_empty()
				&& response.to_device.is_empty();

			if !empty || full_state {
				return Ok(response);
			}
		}

		// Wait for activity
		if time::timeout_at(stop_at, watchers).await.is_err() || services.server.is_stopping() {
			let response =
				build_empty_response(&services, sender_user, sender_device, next_batch).await;

			trace!(since, next_batch, "empty response");
			return Ok(response);
		}

		trace!(
			since,
			last_batch = ?next_batch,
			count = ?services.globals.pending_count(),
			stop_at = ?stop_at,
			"notified by watcher"
		);

		since = next_batch;
	}
}

async fn build_empty_response(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	next_batch: u64,
) -> sync_events::v3::Response {
	let device_one_time_keys_count = sender_device.map_async(|sender_device| {
		services
			.users
			.count_one_time_keys(sender_user, sender_device)
	});

	let device_unused_fallback_key_types = sender_device.map_async(|sender_device| {
		services
			.users
			.unused_fallback_key_algorithms(sender_user, sender_device)
			.collect::<Vec<_>>()
	});

	let (device_one_time_keys_count, device_unused_fallback_key_types) =
		join(device_one_time_keys_count, device_unused_fallback_key_types).await;

	sync_events::v3::Response {
		device_one_time_keys_count: device_one_time_keys_count.unwrap_or_default(),
		device_unused_fallback_key_types,
		..sync_events::v3::Response::new(next_batch.to_string())
	}
}

#[tracing::instrument(
	name = "build",
	level = INFO_SPAN_LEVEL,
	skip_all,
	fields(
		%since,
		%next_batch,
		count = ?services.globals.pending_count(),
    )
)]
#[expect(clippy::too_many_arguments)]
async fn build_sync_events(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	since: u64,
	next_batch: u64,
	full_state: bool,
	state_after: StateAfter,
	filter: &FilterDefinition,
) -> Result<sync_events::v3::Response> {
	// MSC4380: when m.invite_permission_config blocks invites, suppress stored
	// invite events from /sync entirely; a later unblock re-exposes them.
	let invites_blocked = services.users.invites_blocked(sender_user).await;

	let joined_rooms = collect_joined_rooms(
		services,
		sender_user,
		sender_device,
		since,
		next_batch,
		full_state,
		state_after,
		filter,
	);

	let left_rooms = collect_left_rooms(
		services,
		sender_user,
		since,
		next_batch,
		full_state,
		state_after,
		filter,
	);

	let invited_rooms =
		collect_invited_rooms(services, sender_user, since, next_batch, filter, invites_blocked);

	let knocked_rooms = collect_knocked_rooms(services, sender_user, since, next_batch, filter);

	let presence_updates = services
		.config
		.allow_local_presence
		.then_async(|| {
			process_presence_updates(services, since, next_batch, sender_user, filter)
		});

	let account_data = collect_global_account_data(services, sender_user, since, next_batch);

	let keys_changed = services
		.users
		.keys_changed(sender_user, since, Some(next_batch))
		.map(ToOwned::to_owned)
		.collect::<HashSet<_>>();

	let to_device_events = sender_device.map_async(|sender_device| {
		services
			.users
			.get_to_device_events(sender_user, sender_device, Some(since), Some(next_batch))
			.map(at!(1))
			.collect::<Vec<_>>()
	});

	let device_one_time_keys_count = sender_device.map_async(|sender_device| {
		services
			.users
			.count_one_time_keys(sender_user, sender_device)
	});

	let device_unused_fallback_key_types = sender_device.map_async(|sender_device| {
		services
			.users
			.unused_fallback_key_algorithms(sender_user, sender_device)
			.collect::<Vec<_>>()
	});

	// Remove all to-device events the device received *last time*
	let remove_to_device_events = sender_device.map_async(|sender_device| {
		services
			.users
			.remove_to_device_events(sender_user, sender_device, since)
	});

	let (
		account_data,
		keys_changed,
		presence_updates,
		(_, to_device_events, device_one_time_keys_count, device_unused_fallback_key_types),
		(
			(joined_rooms, mut device_list_updates, left_encrypted_users),
			left_rooms,
			invited_rooms,
			knocked_rooms,
		),
	) = join5(
		account_data,
		keys_changed,
		presence_updates,
		join4(
			remove_to_device_events,
			to_device_events,
			device_one_time_keys_count,
			device_unused_fallback_key_types,
		),
		join4(joined_rooms, left_rooms, invited_rooms, knocked_rooms),
	)
	.boxed()
	.await;

	device_list_updates.extend(keys_changed);

	let device_list_left =
		collect_device_list_left(services, sender_user, left_encrypted_users).await;

	let presence_events = build_presence_events(presence_updates);

	Ok(sync_events::v3::Response {
		account_data: GlobalAccountData { events: account_data },
		device_lists: DeviceLists {
			left: device_list_left,
			changed: device_list_updates.into_iter().collect(),
		},
		device_one_time_keys_count: device_one_time_keys_count.unwrap_or_default(),
		device_unused_fallback_key_types,
		next_batch: next_batch.to_string(),
		presence: Presence { events: presence_events },
		rooms: Rooms {
			leave: left_rooms,
			join: joined_rooms,
			invite: invited_rooms,
			knock: knocked_rooms,
		},
		to_device: ToDevice {
			events: to_device_events.unwrap_or_default(),
		},
	})
}

#[expect(clippy::too_many_arguments)]
fn collect_joined_rooms<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	sender_device: Option<&'a DeviceId>,
	since: u64,
	next_batch: u64,
	full_state: bool,
	state_after: StateAfter,
	filter: &'a FilterDefinition,
) -> impl Future<
	Output = (BTreeMap<OwnedRoomId, JoinedRoom>, HashSet<OwnedUserId>, HashSet<OwnedUserId>),
> + Send
+ 'a {
	services
		.state_cache
		.rooms_joined(sender_user)
		.ready_filter(|&room_id| filter.room.matches(room_id))
		.map(ToOwned::to_owned)
		.broad_filter_map(move |room_id| {
			load_joined_room(
				services,
				sender_user,
				sender_device,
				room_id.clone(),
				since,
				next_batch,
				full_state,
				state_after,
				filter,
			)
			.map_ok(move |(joined_room, dlu, jeu)| (room_id, joined_room, dlu, jeu))
			.ok()
		})
		.ready_fold(
			(BTreeMap::new(), HashSet::new(), HashSet::new()),
			|(mut joined_rooms, mut device_list_updates, mut left_encrypted_users),
			 (room_id, joined_room, dlu, leu)| {
				device_list_updates.extend(dlu);
				left_encrypted_users.extend(leu);
				if !joined_room.is_empty() {
					joined_rooms.insert(room_id, joined_room);
				}

				(joined_rooms, device_list_updates, left_encrypted_users)
			},
		)
}

fn collect_left_rooms<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	since: u64,
	next_batch: u64,
	full_state: bool,
	state_after: StateAfter,
	filter: &'a FilterDefinition,
) -> impl Future<Output = BTreeMap<OwnedRoomId, LeftRoom>> + Send + 'a {
	services
		.state_cache
		.rooms_left_state(sender_user)
		.ready_filter(|(room_id, _)| filter.room.matches(room_id))
		.broad_filter_map(move |(room_id, _)| {
			handle_left_room(
				services,
				since,
				room_id.clone(),
				sender_user,
				next_batch,
				full_state,
				state_after,
				filter,
			)
			.map_ok(move |left_room| (room_id, left_room))
			.ok()
		})
		.ready_filter_map(|(room_id, left_room)| left_room.map(|left_room| (room_id, left_room)))
		.collect()
}

async fn collect_invited_rooms<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	since: u64,
	next_batch: u64,
	filter: &'a FilterDefinition,
	invites_blocked: bool,
) -> BTreeMap<OwnedRoomId, InvitedRoom> {
	services
		.state_cache
		.rooms_invited_state(sender_user)
		.ready_filter(move |_| !invites_blocked)
		.ready_filter(|(room_id, _)| filter.room.matches(room_id))
		.fold_default(async |mut invited_rooms: BTreeMap<_, _>, (room_id, invite_state)| {
			let invite_count = services
				.state_cache
				.get_invite_count(&room_id, sender_user)
				.await
				.ok();

			// Invited before last sync
			if Some(since) >= invite_count || Some(next_batch) < invite_count {
				return invited_rooms;
			}

			let invited_room = InvitedRoom {
				invite_state: InviteState { events: invite_state },
			};

			invited_rooms.insert(room_id, invited_room);
			invited_rooms
		})
		.await
}

async fn collect_knocked_rooms<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	since: u64,
	next_batch: u64,
	filter: &'a FilterDefinition,
) -> BTreeMap<OwnedRoomId, KnockedRoom> {
	services
		.state_cache
		.rooms_knocked_state(sender_user)
		.ready_filter(|(room_id, _)| filter.room.matches(room_id))
		.fold_default(async |mut knocked_rooms: BTreeMap<_, _>, (room_id, knock_state)| {
			let knock_count = services
				.state_cache
				.get_knock_count(&room_id, sender_user)
				.await
				.ok();

			// Knocked before last sync; or after the cutoff for this sync
			if Some(since) >= knock_count || Some(next_batch) < knock_count {
				return knocked_rooms;
			}

			let knocked_room = KnockedRoom {
				knock_state: KnockState { events: knock_state },
			};

			knocked_rooms.insert(room_id, knocked_room);
			knocked_rooms
		})
		.await
}

fn collect_global_account_data<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	since: u64,
	next_batch: u64,
) -> impl Future<Output = Vec<Raw<AnyGlobalAccountDataEvent>>> + Send + 'a {
	services
		.account_data
		.changes_since(None, sender_user, since, Some(next_batch))
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Global))
		.ready_filter(move |e| since != 0 || !is_empty_account_data_event(e))
		.collect()
}

fn collect_device_list_left<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	left_encrypted_users: HashSet<OwnedUserId>,
) -> impl Future<Output = Vec<OwnedUserId>> + Send + 'a {
	left_encrypted_users
		.into_iter()
		.stream()
		.broad_filter_map(async |user_id: OwnedUserId| {
			share_encrypted_room(services, sender_user, &user_id, None)
				.await
				.eq(&false)
				.then_some(user_id)
		})
		.collect()
}

fn build_presence_events(presence_updates: Option<PresenceUpdates>) -> Vec<Raw<PresenceEvent>> {
	presence_updates
		.into_iter()
		.flat_map(IntoIterator::into_iter)
		.map(|(sender, content)| PresenceEvent { content, sender })
		.map(|ref event| Raw::new(event))
		.filter_map(Result::ok)
		.collect()
}

#[tracing::instrument(name = "presence", level = "debug", skip_all)]
async fn process_presence_updates(
	services: &Services,
	since: u64,
	next_batch: u64,
	syncing_user: &UserId,
	filter: &FilterDefinition,
) -> PresenceUpdates {
	services
		.presence
		.presence_since(since, Some(next_batch))
		.ready_filter(|(user_id, ..)| filter.presence.matches(user_id))
		.filter(|(user_id, ..)| {
			services
				.state_cache
				.user_sees_user(syncing_user, user_id)
		})
		.filter_map(|(user_id, _, presence_bytes)| {
			services
				.presence
				.from_json_bytes_to_event(presence_bytes, user_id)
				.map_ok(move |event| (user_id, event))
				.ok()
		})
		.map(|(user_id, event)| (user_id.to_owned(), event.content))
		.collect()
		.boxed()
		.await
}

#[tracing::instrument(
	name = "left",
	level = "debug",
	skip_all,
	fields(
		room_id = %room_id,
		full = %full_state,
	),
)]
#[expect(clippy::too_many_arguments)]
async fn handle_left_room(
	services: &Services,
	since: u64,
	ref room_id: OwnedRoomId,
	sender_user: &UserId,
	next_batch: u64,
	full_state: bool,
	state_after: StateAfter,
	filter: &FilterDefinition,
) -> Result<Option<LeftRoom>> {
	let left_count = services
		.state_cache
		.get_left_count(room_id, sender_user)
		.await
		.unwrap_or(0);

	if left_count == 0 || left_count > next_batch {
		return Ok(None);
	}

	let include_leave = filter.room.include_leave;
	if since == 0 && !include_leave {
		return Ok(None);
	}

	// Cannot sync unless the event falls within the snapshot. The room is only
	// sync'ed once to the client, after that it's too late.
	if since != 0 && left_count <= since {
		return Ok(None);
	}

	let is_not_found = services.metadata.exists(room_id).is_false();

	let is_disabled = services.metadata.is_disabled(room_id);

	let is_banned = services.metadata.is_banned(room_id);

	pin_mut!(is_not_found, is_disabled, is_banned);
	if is_not_found.or(is_disabled).or(is_banned).await {
		// For rejected invites, deleted, missing, or broken room state this is the last
		// resort to convey a the minimum of information to the client.
		let event = PduEvent {
			event_id: EventId::new_v1(services.globals.server_name()),
			origin_server_ts: utils::millis_since_unix_epoch().try_into()?,
			kind: RoomMember,
			state_key: Some(sender_user.as_str().into()),
			sender: sender_user.to_owned(),
			content: serde_json::from_str(r#"{"membership":"leave"}"#)?,
			// The following keys are dropped on conversion
			room_id: room_id.clone(),
			depth: uint!(1),
			origin: None,
			unsigned: None,
			redacts: None,
			hashes: EventHash::default(),
			auth_events: Default::default(),
			prev_events: Default::default(),
		};

		let state = state_after.wrap(StateEvents {
			events: vec![trim_event_fields(event.into_format(), filter.event_fields.as_deref())],
		});

		return Ok(Some(LeftRoom {
			account_data: RoomAccountData::default(),
			state,
			timeline: Timeline {
				limited: false,
				events: Default::default(),
				prev_batch: Some(left_count.to_string()),
			},
		}));
	}

	load_left_room(
		services,
		sender_user,
		room_id,
		since,
		left_count,
		full_state,
		state_after,
		filter,
	)
	.await
}

#[tracing::instrument(name = "load", level = "debug", skip_all)]
#[expect(clippy::too_many_arguments)]
async fn load_left_room(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	since: u64,
	left_count: u64,
	full_state: bool,
	state_after: StateAfter,
	filter: &FilterDefinition,
) -> Result<Option<LeftRoom>> {
	let initial = since == 0;
	let timeline_limit: usize = filter
		.room
		.timeline
		.limit
		.map(TryInto::try_into)
		.map_expect("UInt to usize")
		.unwrap_or(10)
		.min(100);

	let (timeline_pdus, limited, _) = load_timeline(
		services,
		sender_user,
		room_id,
		PduCount::Normal(since),
		Some(PduCount::Normal(left_count)),
		timeline_limit.max(1),
	)
	.await
	.unwrap_or_default();

	let since_shortstatehash = services
		.timeline
		.prev_shortstatehash(room_id, PduCount::Normal(since).saturating_add(1))
		.ok();

	let horizon_shortstatehash = timeline_pdus
		.first()
		.map(at!(0))
		.map_async(|count| {
			services
				.timeline
				.get_shortstatehash(room_id, count)
				.inspect_err(inspect_debug_log)
				.ok()
		});

	// MSC4222 `state_after`: state at the leave (end of timeline). The
	// stored shortstatehash at the leave PDU is state-before-leave, so
	// step to the next PDU; if no event followed, the room's current
	// shortstatehash is the post-leave state.
	let after_shortstatehash = state_after.requested().then_async(|| {
		services
			.timeline
			.next_shortstatehash(room_id, PduCount::Normal(left_count))
			.or_else(|_| services.state.get_room_shortstatehash(room_id))
			.inspect_err(inspect_debug_log)
	});

	let left_shortstatehash = services
		.timeline
		.get_shortstatehash(room_id, PduCount::Normal(left_count))
		.inspect_err(inspect_debug_log)
		.or_else(|_| services.state.get_room_shortstatehash(room_id))
		.map_err(|_| err!(Database(error!("Room {room_id} has no state"))));

	let (since_shortstatehash, horizon_shortstatehash, after_shortstatehash, left_shortstatehash) =
		join4(
			since_shortstatehash,
			horizon_shortstatehash,
			after_shortstatehash,
			left_shortstatehash,
		)
		.boxed()
		.await;

	let StateChanges { state_events, .. } = calculate_state_changes(
		services,
		sender_user,
		room_id,
		full_state || initial,
		state_after,
		since_shortstatehash,
		horizon_shortstatehash.flatten(),
		after_shortstatehash.flat_ok(),
		left_shortstatehash?,
		false,
		None,
	)
	.boxed()
	.await?;

	let is_sender_membership = |event: &PduEvent| {
		*event.kind() == RoomMember && event.state_key() == Some(sender_user.as_str())
	};

	let timeline_sender_member = timeline_limit
		.eq(&0)
		.then(|| timeline_pdus.last().map(ref_at!(1)).cloned())
		.into_iter()
		.flat_map(Option::into_iter);

	let encrypted = services
		.state_accessor
		.is_encrypted_room(room_id)
		.await;

	let event_fields = filter.event_fields.as_deref();

	let in_timeline = in_timeline(&timeline_pdus);

	let state_events = state_events
		.into_iter()
		.filter(|pdu| filter.room.state.matches(pdu))
		.filter(|pdu| timeline_limit > 0 || !is_sender_membership(pdu))
		.chain(timeline_sender_member)
		.stream()
		.wide_then(|pdu| with_membership(services, pdu, sender_user, encrypted))
		.map(|pdu| strip_prev_state(pdu, sender_user, &in_timeline))
		.map(|pdu| trim_event_fields(pdu.into_format(), event_fields))
		.collect();

	let left_prev_batch = timeline_limit
		.eq(&0)
		.then_some(left_count)
		.map(PduCount::Normal);

	let prev_batch = timeline_pdus
		.first()
		.filter(|_| timeline_limit > 0)
		.map(at!(0))
		.or(left_prev_batch)
		.as_ref()
		.map(ToString::to_string);

	let timeline_events = timeline_pdus
		.into_iter()
		.stream()
		.wide_filter_map(|item| ignored_filter(services, item, sender_user))
		.map(at!(1))
		.ready_filter(|pdu| filter.room.timeline.matches(pdu))
		.take(timeline_limit)
		.wide_then(|pdu| with_membership(services, pdu, sender_user, encrypted))
		.wide_then(|pdu| {
			services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
		})
		.collect::<Vec<_>>();

	let account_data_events = services
		.account_data
		.changes_since(Some(room_id), sender_user, since, None)
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Room))
		.ready_filter(move |e| since != 0 || !is_empty_account_data_event(e))
		.collect();

	let (state_events, account_data_events, timeline_events) =
		join3(state_events, account_data_events, timeline_events)
			.boxed()
			.await;

	let state = state_after.wrap(StateEvents { events: state_events });

	Ok(Some(LeftRoom {
		account_data: RoomAccountData { events: account_data_events },
		state,
		timeline: Timeline {
			prev_batch,
			limited: limited || timeline_limit == 0,
			events: timeline_events
				.into_iter()
				.map(|pdu| trim_event_fields(pdu.into_format(), event_fields))
				.collect(),
		},
	}))
}

fn in_timeline(timeline_pdus: &[(PduCount, PduEvent)]) -> impl Fn(&PduEvent) -> bool + use<> {
	let timeline_ids: TimelineEventIds = timeline_pdus
		.iter()
		.map(ref_at!(1))
		.map(Event::event_id)
		.map(ToOwned::to_owned)
		.collect();

	move |event: &PduEvent| {
		timeline_ids
			.iter()
			.any(is_equal_to!(event.event_id()))
	}
}

#[tracing::instrument(
	name = "joined",
	level = "debug",
	skip_all,
	fields(
		room_id = ?room_id,
	),
)]
#[expect(clippy::too_many_arguments)]
async fn load_joined_room(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	ref room_id: OwnedRoomId,
	since: u64,
	next_batch: u64,
	full_state: bool,
	state_after: StateAfter,
	filter: &FilterDefinition,
) -> Result<(JoinedRoom, HashSet<OwnedUserId>, HashSet<OwnedUserId>)> {
	let initial = since == 0;
	let (timeline_pdus, limited, last_timeline_count) =
		load_join_timeline(services, sender_user, room_id, since, next_batch, filter).await?;

	let timeline_changed = last_timeline_count.into_unsigned() > since;
	debug_assert!(
		timeline_pdus.is_empty() || timeline_changed,
		"if timeline events, last_timeline_count must be in the since window."
	);

	let RoomMetadata {
		since_shortstatehash,
		horizon_shortstatehash,
		after_shortstatehash,
		current_shortstatehash,
		receipt_events,
		encrypted_room,
	} = gather_room_metadata(
		services,
		sender_user,
		room_id,
		since,
		next_batch,
		&timeline_pdus,
		last_timeline_count,
		timeline_changed,
		state_after,
	)
	.boxed()
	.await?;

	let UserMetadata {
		witness,
		last_notification_read,
		thread_last_reads,
		last_privateread_update,
		joined_since_last_sync,
	} = gather_user_metadata(
		services,
		sender_user,
		sender_device,
		room_id,
		filter,
		&timeline_pdus,
		&receipt_events,
		since,
		initial,
		timeline_changed,
		encrypted_room,
		since_shortstatehash,
	)
	.boxed()
	.await;

	let StateChanges {
		heroes,
		joined_member_count,
		invited_member_count,
		mut state_events,
	} = compute_join_state_changes(
		services,
		sender_user,
		room_id,
		full_state || initial,
		state_after,
		since_shortstatehash,
		horizon_shortstatehash,
		after_shortstatehash,
		current_shortstatehash,
		joined_since_last_sync,
		witness.as_ref(),
	)
	.await?;

	let joined_sender_member = take_sender_membership_for_join(
		&mut state_events,
		sender_user,
		joined_since_last_sync,
		timeline_pdus.is_empty(),
		initial,
	);

	let prev_batch =
		compute_join_prev_batch(&timeline_pdus, joined_sender_member.as_ref(), since);
	let in_window = |count: u64| count > since && count <= next_batch;

	let NotificationGates {
		send_notification_counts,
		send_notification_count_filter,
	} = compute_notification_gates(
		last_notification_read,
		thread_last_reads.as_ref(),
		since,
		in_window,
	);

	// `encrypted_room` is `Some` whenever timeline or state events are emitted.
	let encrypted = encrypted_room.unwrap_or(false);

	let aggregates = await_join_aggregates(
		services,
		sender_user,
		room_id,
		&state_events,
		timeline_pdus,
		joined_sender_member,
		encrypted,
		initial,
		since,
		next_batch,
		last_privateread_update,
		send_notification_counts,
		filter,
	)
	.await;

	let (joined_room, device_list_updates, left_encrypted_users) = finalize_joined_room(
		services,
		sender_user,
		filter,
		state_events,
		aggregates,
		receipt_events,
		heroes,
		joined_member_count,
		invited_member_count,
		thread_last_reads.as_ref(),
		send_notification_count_filter,
		FinalizeJoinFlags {
			encrypted,
			full_state,
			state_after,
			limited,
			joined_since_last_sync,
			initial,
		},
		in_window,
		prev_batch,
	)
	.await;

	Ok((joined_room, device_list_updates, left_encrypted_users))
}

#[expect(clippy::too_many_arguments)]
async fn compute_join_state_changes(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	full_state: bool,
	state_after: StateAfter,
	since_shortstatehash: Option<ShortStateHash>,
	horizon_shortstatehash: Option<ShortStateHash>,
	after_shortstatehash: Option<ShortStateHash>,
	current_shortstatehash: Option<ShortStateHash>,
	joined_since_last_sync: bool,
	witness: Option<&Witness>,
) -> Result<StateChanges> {
	current_shortstatehash
		.map_async(|current_shortstatehash| {
			calculate_state_changes(
				services,
				sender_user,
				room_id,
				full_state,
				state_after,
				since_shortstatehash,
				horizon_shortstatehash,
				after_shortstatehash,
				current_shortstatehash,
				joined_since_last_sync,
				witness,
			)
		})
		.await
		.transpose()
		.map(Option::unwrap_or_default)
}

fn compute_join_prev_batch(
	timeline_pdus: &[(PduCount, PduEvent)],
	joined_sender_member: Option<&PduEvent>,
	since: u64,
) -> Option<PduCount> {
	timeline_pdus.first().map(at!(0)).or_else(|| {
		joined_sender_member
			.is_some()
			.then_some(since)
			.map(Into::into)
	})
}

#[expect(clippy::too_many_arguments)]
async fn assemble_join_state_events(
	services: &Services,
	state_events: Vec<PduEvent>,
	sender_user: &UserId,
	encrypted: bool,
	room_events: &[PduEvent],
	filter: &FilterDefinition,
	full_state: bool,
	state_after: StateAfter,
) -> Vec<Raw<AnySyncStateEvent>> {
	let is_in_timeline = |event: &PduEvent| {
		room_events
			.iter()
			.map(Event::event_id)
			.any(is_equal_to!(event.event_id()))
	};

	// MSC4222: when the client opts into `state_after`, state events that
	// took effect within the timeline appear in both the timeline and the
	// state section, so the in-timeline exclusion is bypassed.
	let include_in_state = |event: &PduEvent| {
		let filter = &filter.room.state;
		filter.matches(event) && (full_state || state_after.requested() || !is_in_timeline(event))
	};

	assemble_state_events(
		services,
		state_events,
		sender_user,
		encrypted,
		include_in_state,
		&is_in_timeline,
		filter.event_fields.as_deref(),
	)
	.await
}

async fn load_join_timeline(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	since: u64,
	next_batch: u64,
	filter: &FilterDefinition,
) -> Result<(Vec<(PduCount, PduEvent)>, bool, PduCount)> {
	let timeline_limit: usize = filter
		.room
		.timeline
		.limit
		.map(TryInto::try_into)
		.map_expect("UInt to usize")
		.unwrap_or(10)
		.min(100);

	load_timeline(
		services,
		sender_user,
		room_id,
		PduCount::Normal(since),
		Some(PduCount::Normal(next_batch)),
		timeline_limit,
	)
	.await
}

struct JoinAggregates {
	room_events: Vec<PduEvent>,
	account_data_events: Vec<Raw<AnyRoomAccountDataEvent>>,
	typing_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
	private_read_events: Option<PrivateReadEvents>,
	notification_count: Option<UInt>,
	highlight_count: Option<UInt>,
	thread_counts: Option<BTreeMap<OwnedEventId, (u64, u64)>>,
	device_list_updates: HashSet<OwnedUserId>,
	left_encrypted_users: HashSet<OwnedUserId>,
}

#[expect(clippy::too_many_arguments)]
async fn await_join_aggregates(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	state_events: &[PduEvent],
	timeline_pdus: Vec<(PduCount, PduEvent)>,
	joined_sender_member: Option<PduEvent>,
	encrypted: bool,
	initial: bool,
	since: u64,
	next_batch: u64,
	last_privateread_update: u64,
	send_notification_counts: bool,
	filter: &FilterDefinition,
) -> JoinAggregates {
	let (notification_count, highlight_count, thread_counts) =
		notification_count_futures(services, sender_user, room_id, send_notification_counts);

	let private_read_events = last_privateread_update.gt(&since).then_async(|| {
		services
			.read_receipt
			.private_read_get(room_id, sender_user)
			.unwrap_or_default()
	});

	let typing_events = gather_typing_events(services, room_id, sender_user, since);

	let device_list_updates = gather_device_list_updates(
		services,
		sender_user,
		room_id,
		timeline_membership_changes(&timeline_pdus, initial),
		state_events,
		initial,
		since,
		next_batch,
	);

	let room_events = collect_room_events(
		services,
		sender_user,
		timeline_pdus,
		joined_sender_member,
		encrypted,
		filter,
	);

	let account_data_events = collect_room_account_data(services, sender_user, room_id, since);

	let (
		(room_events, account_data_events),
		(typing_events, private_read_events),
		(notification_count, highlight_count, thread_counts),
		(device_list_updates, left_encrypted_users),
	) = join4(
		join(room_events, account_data_events),
		join(typing_events, private_read_events),
		join3(notification_count, highlight_count, thread_counts),
		device_list_updates,
	)
	.boxed()
	.await;

	JoinAggregates {
		room_events,
		account_data_events,
		typing_events,
		private_read_events,
		notification_count,
		highlight_count,
		thread_counts,
		device_list_updates,
		left_encrypted_users,
	}
}

struct FinalizeJoinFlags {
	encrypted: bool,
	full_state: bool,
	state_after: StateAfter,
	limited: bool,
	joined_since_last_sync: bool,
	initial: bool,
}

#[expect(clippy::too_many_arguments)]
async fn finalize_joined_room(
	services: &Services,
	sender_user: &UserId,
	filter: &FilterDefinition,
	state_events: Vec<PduEvent>,
	aggregates: JoinAggregates,
	receipt_events: Vec<(OwnedUserId, Raw<AnySyncEphemeralRoomEvent>)>,
	heroes: Option<Vec<OwnedUserId>>,
	joined_member_count: Option<u64>,
	invited_member_count: Option<u64>,
	thread_last_reads: Option<&BTreeMap<OwnedEventId, u64>>,
	send_notification_count_filter: impl Fn(&UInt) -> bool,
	flags: FinalizeJoinFlags,
	in_window: impl Fn(u64) -> bool,
	prev_batch: Option<PduCount>,
) -> (JoinedRoom, HashSet<OwnedUserId>, HashSet<OwnedUserId>) {
	let JoinAggregates {
		room_events,
		account_data_events,
		typing_events,
		private_read_events,
		notification_count,
		highlight_count,
		thread_counts,
		device_list_updates,
		left_encrypted_users,
	} = aggregates;

	let FinalizeJoinFlags {
		encrypted,
		full_state,
		state_after,
		limited,
		joined_since_last_sync,
		initial,
	} = flags;

	let state_events = assemble_join_state_events(
		services,
		state_events,
		sender_user,
		encrypted,
		&room_events,
		filter,
		full_state,
		state_after,
	)
	.await;

	let (unread_notifications, unread_thread_notifications) = assemble_unread_notifications(
		notification_count,
		highlight_count,
		thread_counts,
		thread_last_reads,
		send_notification_count_filter,
		filter.room.timeline.unread_thread_notifications,
		initial,
		in_window,
	);

	let joined_room = build_joined_room(
		BuildJoinedRoom {
			receipt_events,
			typing_events,
			private_read_events,
			state_events,
			account_data_events,
			room_events,
			heroes,
			joined_member_count,
			invited_member_count,
			unread_notifications,
			unread_thread_notifications,
			state_after,
			limited,
			joined_since_last_sync,
			prev_batch,
		},
		filter.event_fields.as_deref(),
	);

	(joined_room, device_list_updates, left_encrypted_users)
}

fn build_joined_room(args: BuildJoinedRoom, event_fields: Option<&[String]>) -> JoinedRoom {
	let BuildJoinedRoom {
		receipt_events,
		typing_events,
		private_read_events,
		state_events,
		account_data_events,
		room_events,
		heroes,
		joined_member_count,
		invited_member_count,
		unread_notifications,
		unread_thread_notifications,
		state_after,
		limited,
		joined_since_last_sync,
		prev_batch,
	} = args;

	let edus: Vec<Raw<AnySyncEphemeralRoomEvent>> = receipt_events
		.into_iter()
		.map(at!(1))
		.chain(typing_events)
		.chain(private_read_events.into_iter().flatten())
		.collect();

	let state = state_after.wrap(StateEvents { events: state_events });

	let heroes = heroes
		.into_iter()
		.flatten()
		.map(TryInto::try_into)
		.filter_map(Result::ok)
		.collect();

	JoinedRoom {
		account_data: RoomAccountData { events: account_data_events },
		ephemeral: Ephemeral { events: edus },
		state,
		summary: RoomSummary {
			joined_member_count: joined_member_count.map(ruma_from_u64),
			invited_member_count: invited_member_count.map(ruma_from_u64),
			heroes,
		},
		timeline: Timeline {
			limited: limited || joined_since_last_sync,
			prev_batch: prev_batch.as_ref().map(ToString::to_string),
			events: room_events
				.into_iter()
				.map(|pdu| trim_event_fields(pdu.into_format(), event_fields))
				.collect(),
		},
		unread_notifications,
		unread_thread_notifications,
	}
}

#[expect(clippy::too_many_arguments)]
async fn gather_room_metadata(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	since: u64,
	next_batch: u64,
	timeline_pdus: &[(PduCount, PduEvent)],
	last_timeline_count: PduCount,
	timeline_changed: bool,
	state_after: StateAfter,
) -> Result<RoomMetadata> {
	let since_shortstatehash = timeline_changed.then_async(|| {
		services
			.timeline
			.prev_shortstatehash(room_id, PduCount::Normal(since).saturating_add(1))
			.ok()
	});

	let horizon_shortstatehash = timeline_pdus
		.first()
		.map(at!(0))
		.map_async(|count| {
			services
				.timeline
				.get_shortstatehash(room_id, count)
				.inspect_err(inspect_debug_log)
		});

	// MSC4222 `state_after` semantics: state at the *end* of the timeline
	// window. `next_shortstatehash` reads state-before the next PDU, which
	// equals state-after our last PDU; falling back to the room's current
	// state covers the case where our window already touches HEAD.
	let after_shortstatehash = state_after.requested().then_async(|| {
		services
			.timeline
			.next_shortstatehash(room_id, last_timeline_count)
			.or_else(|_| services.state.get_room_shortstatehash(room_id))
			.inspect_err(inspect_debug_log)
	});

	let current_shortstatehash = timeline_changed.then_async(|| {
		services
			.timeline
			.get_shortstatehash(room_id, last_timeline_count)
			.inspect_err(inspect_debug_log)
			.or_else(|_| services.state.get_room_shortstatehash(room_id))
			.map_err(|_| err!(Database(error!("Room {room_id} has no state"))))
	});

	let encrypted_room =
		timeline_changed.then_async(|| services.state_accessor.is_encrypted_room(room_id));

	let receipt_events = services
		.read_receipt
		.readreceipts_since(room_id, since, Some(next_batch))
		.filter_map(async |(read_user, _, edu)| {
			services
				.users
				.user_is_ignored(read_user, sender_user)
				.await
				.or_some((read_user.to_owned(), edu))
		})
		.collect::<Vec<(OwnedUserId, Raw<AnySyncEphemeralRoomEvent>)>>();

	let (
		(
			since_shortstatehash,
			horizon_shortstatehash,
			after_shortstatehash,
			current_shortstatehash,
		),
		receipt_events,
		encrypted_room,
	) = join3(
		join4(
			since_shortstatehash,
			horizon_shortstatehash,
			after_shortstatehash,
			current_shortstatehash,
		),
		receipt_events,
		encrypted_room,
	)
	.boxed()
	.await;

	Ok(RoomMetadata {
		since_shortstatehash: since_shortstatehash.flatten(),
		horizon_shortstatehash: horizon_shortstatehash.flat_ok(),
		after_shortstatehash: after_shortstatehash.flat_ok(),
		current_shortstatehash: current_shortstatehash.transpose()?,
		receipt_events,
		encrypted_room,
	})
}

fn collect_room_events<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	timeline_pdus: Vec<(PduCount, PduEvent)>,
	joined_sender_member: Option<PduEvent>,
	encrypted: bool,
	filter: &'a FilterDefinition,
) -> impl Future<Output = Vec<PduEvent>> + Send + 'a {
	let include_in_timeline = |event: &PduEvent| filter.room.timeline.matches(event);
	timeline_pdus
		.into_iter()
		.stream()
		.wide_filter_map(|item| ignored_filter(services, item, sender_user))
		.map(at!(1))
		.chain(joined_sender_member.into_iter().stream())
		.ready_filter(include_in_timeline)
		.wide_then(move |pdu| with_membership(services, pdu, sender_user, encrypted))
		.wide_then(move |pdu| {
			services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
		})
		.collect::<Vec<_>>()
}

fn collect_room_account_data<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	room_id: &'a RoomId,
	since: u64,
) -> impl Future<Output = Vec<Raw<AnyRoomAccountDataEvent>>> + Send + 'a {
	services
		.account_data
		.changes_since(Some(room_id), sender_user, since, None)
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Room))
		.ready_filter(move |e| since != 0 || !is_empty_account_data_event(e))
		.collect()
}

#[expect(clippy::type_complexity)]
fn notification_count_futures<'a>(
	services: &'a Services,
	sender_user: &'a UserId,
	room_id: &'a RoomId,
	send: bool,
) -> (
	impl Future<Output = Option<UInt>> + Send + 'a,
	impl Future<Output = Option<UInt>> + Send + 'a,
	impl Future<Output = Option<BTreeMap<OwnedEventId, (u64, u64)>>> + Send + 'a,
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

	// MSC3773: per-thread counts. Filtered downstream by per-thread last-read
	// so quiet threads are omitted on rounds where the main cursor advanced.
	let thread_counts = send.then_async(move || {
		services
			.pusher
			.thread_notification_counts(sender_user, room_id)
	});

	(notification_count, highlight_count, thread_counts)
}

fn take_sender_membership_for_join(
	state_events: &mut Vec<PduEvent>,
	sender_user: &UserId,
	joined_since_last_sync: bool,
	timeline_empty: bool,
	initial: bool,
) -> Option<PduEvent> {
	if !(joined_since_last_sync && timeline_empty && !initial) {
		return None;
	}

	let is_sender_membership = |event: &PduEvent| {
		*event.event_type() == StateEventType::RoomMember.into()
			&& event
				.state_key()
				.is_some_and(is_equal_to!(sender_user.as_str()))
	};

	state_events
		.iter()
		.position(is_sender_membership)
		.map(|pos| state_events.swap_remove(pos))
}

#[expect(clippy::too_many_arguments)]
async fn gather_user_metadata(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	room_id: &RoomId,
	filter: &FilterDefinition,
	timeline_pdus: &[(PduCount, PduEvent)],
	receipt_events: &[(OwnedUserId, Raw<AnySyncEphemeralRoomEvent>)],
	since: u64,
	initial: bool,
	timeline_changed: bool,
	encrypted_room: Option<bool>,
	since_shortstatehash: Option<ShortStateHash>,
) -> UserMetadata {
	let lazy_load_options =
		[&filter.room.state.lazy_load_options, &filter.room.timeline.lazy_load_options];

	let lazy_loading_enabled = encrypted_room.is_some_and(is_false!())
		&& lazy_load_options
			.iter()
			.any(|opts| opts.is_enabled());

	let lazy_loading_context = &lazy_loading::Context {
		user_id: sender_user,
		device_id: sender_device,
		room_id,
		token: Some(since),
		options: Some(&filter.room.state.lazy_load_options),
		mode: lazy_loading::Mode::Update,
	};

	// Reset lazy loading because this is an initial sync
	let lazy_load_reset =
		initial.then_async(|| services.lazy_loading.reset(lazy_loading_context));

	lazy_load_reset.await;
	let witness = lazy_loading_enabled.then_async(|| {
		let witness: Witness = timeline_pdus
			.iter()
			.map(ref_at!(1))
			.map(Event::sender)
			.map(Into::into)
			.chain(receipt_events.iter().map(ref_at!(0)).cloned())
			.collect();

		services
			.lazy_loading
			.witness_retain(witness, lazy_loading_context)
	});

	let sender_joined_count = timeline_changed.then_async(|| {
		services
			.state_cache
			.get_joined_count(room_id, sender_user)
			.unwrap_or(0)
	});

	let since_encryption = since_shortstatehash.map_async(|shortstatehash| {
		services
			.state_accessor
			.state_get(shortstatehash, &StateEventType::RoomEncryption, "")
	});

	let last_notification_read = timeline_pdus.is_empty().then_async(|| {
		services
			.pusher
			.last_notification_read(sender_user, room_id)
			.ok()
	});

	let thread_last_reads = timeline_pdus.is_empty().then_async(|| {
		services
			.pusher
			.thread_last_notification_reads(sender_user, room_id)
	});

	let last_privateread_update = services
		.read_receipt
		.last_privateread_update(sender_user, room_id);

	let (
		(last_privateread_update, last_notification_read, thread_last_reads),
		(sender_joined_count, since_encryption),
		witness,
	) = join3(
		join3(last_privateread_update, last_notification_read, thread_last_reads),
		join(sender_joined_count, since_encryption),
		witness,
	)
	.await;

	let _encrypted_since_last_sync =
		!initial && encrypted_room.is_some_and(is_true!()) && since_encryption.is_none();

	let joined_since_last_sync = sender_joined_count.unwrap_or(0) > since;

	UserMetadata {
		witness,
		last_notification_read,
		thread_last_reads,
		last_privateread_update,
		joined_since_last_sync,
	}
}

#[expect(clippy::option_option)]
fn compute_notification_gates(
	last_notification_read: Option<Option<u64>>,
	thread_last_reads: Option<&BTreeMap<OwnedEventId, u64>>,
	since: u64,
	in_window: impl Fn(u64) -> bool,
) -> NotificationGates<impl Fn(&UInt) -> bool> {
	let send_main_counts = last_notification_read
		.flatten()
		.is_none_or(&in_window);

	let send_thread_counts =
		thread_last_reads.is_none_or(|reads| reads.values().copied().any(&in_window));

	// Send room-level counts when either the main read cursor or any thread
	// cursor advanced within the window. Thread-only resets do not bump the
	// main cursor, so without the thread leg they would never reach the
	// client.
	let send_notification_counts = send_main_counts || send_thread_counts;

	let send_notification_resets = last_notification_read
		.flatten()
		.is_some_and(|last_count| last_count > since);

	let send_notification_count_filter =
		move |count: &UInt| *count != uint!(0) || send_notification_resets;

	NotificationGates {
		send_notification_counts,
		send_notification_count_filter,
	}
}

async fn gather_typing_events(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
	since: u64,
) -> Vec<Raw<AnySyncEphemeralRoomEvent>> {
	services
		.typing
		.last_typing_update(room_id)
		.and_then(async |count| {
			if count <= since {
				return Ok(Vec::<Raw<AnySyncEphemeralRoomEvent>>::new());
			}

			let typings = typings_event_for_user(services, room_id, sender_user).await?;

			Ok(vec![serde_json::from_str(&serde_json::to_string(&typings)?)?])
		})
		.unwrap_or(Vec::new())
		.await
}

fn timeline_membership_changes(
	timeline_pdus: &[(PduCount, PduEvent)],
	initial: bool,
) -> Vec<(MembershipState, OwnedUserId)> {
	timeline_pdus
		.iter()
		.filter(|_| !initial)
		.map(ref_at!(1))
		.filter_map(extract_membership)
		.collect::<Vec<_>>()
}

fn extract_membership(event: &PduEvent) -> Option<(MembershipState, OwnedUserId)> {
	let content: RoomMemberEventContent = event.get_content().ok()?;
	let user_id: OwnedUserId = event.state_key()?.parse().ok()?;

	Some((content.membership, user_id))
}

#[expect(clippy::too_many_arguments)]
async fn gather_device_list_updates(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	timeline_membership_changes: Vec<(MembershipState, OwnedUserId)>,
	state_events: &[PduEvent],
	initial: bool,
	since: u64,
	next_batch: u64,
) -> (HashSet<OwnedUserId>, HashSet<OwnedUserId>) {
	let keys_changed = services
		.users
		.room_keys_changed(room_id, since, Some(next_batch))
		.map(|(user_id, _)| user_id)
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>();

	let (mut dlu, leu) = state_events
		.iter()
		.stream()
		.ready_filter(|_| !initial)
		.ready_filter(|state_event| *state_event.event_type() == RoomMember)
		.ready_filter_map(extract_membership)
		.chain(timeline_membership_changes.into_iter().stream())
		.fold_default(async |(mut dlu, mut leu): pair_of!(HashSet<_>), (membership, user_id)| {
			use MembershipState::*;

			let requires_update = async |user_id| {
				!share_encrypted_room(services, sender_user, user_id, Some(room_id)).await
			};

			match membership {
				| Join if requires_update(&user_id).await => dlu.insert(user_id),
				| Leave => leu.insert(user_id),
				| _ => false,
			};

			(dlu, leu)
		})
		.await;

	dlu.extend(keys_changed.await);
	(dlu, leu)
}

async fn assemble_state_events(
	services: &Services,
	state_events: Vec<PduEvent>,
	sender_user: &UserId,
	encrypted: bool,
	include_in_state: impl Fn(&PduEvent) -> bool + Send + Sync,
	in_timeline: impl Fn(&PduEvent) -> bool + Send + Sync,
	event_fields: Option<&[String]>,
) -> Vec<Raw<AnySyncStateEvent>> {
	state_events
		.into_iter()
		.filter(include_in_state)
		.stream()
		.wide_then(|pdu| with_membership(services, pdu, sender_user, encrypted))
		.map(|pdu| strip_prev_state(pdu, sender_user, &in_timeline))
		.map(|pdu| trim_event_fields(pdu.into_format(), event_fields))
		.collect()
		.await
}

#[expect(clippy::too_many_arguments)]
fn assemble_unread_notifications(
	notification_count: Option<UInt>,
	highlight_count: Option<UInt>,
	thread_counts: Option<BTreeMap<OwnedEventId, (u64, u64)>>,
	thread_last_reads: Option<&BTreeMap<OwnedEventId, u64>>,
	send_notification_count_filter: impl Fn(&UInt) -> bool,
	want_thread_unread: bool,
	initial: bool,
	in_window: impl Fn(u64) -> bool,
) -> (UnreadNotificationsCount, BTreeMap<OwnedEventId, UnreadNotificationsCount>) {
	let thread_counts = thread_counts.unwrap_or_default();

	let (thread_total_notifications, thread_total_highlights) = thread_counts
		.values()
		.fold((0_u64, 0_u64), |(n, h), &(notifs, hl)| {
			(n.saturating_add(notifs), h.saturating_add(hl))
		});

	// MSC3773: when the client opts in via the timeline filter, partition
	// notification counts per thread. Otherwise sum into the room total.
	let merge_total = |total: u64| {
		move |count: UInt| {
			want_thread_unread
				.is_false()
				.then(|| count.saturating_add(UInt::try_from(total).unwrap_or_default()))
				.unwrap_or(count)
		}
	};

	let unread_notifications = UnreadNotificationsCount {
		highlight_count: highlight_count
			.map(merge_total(thread_total_highlights))
			.filter(&send_notification_count_filter),
		notification_count: notification_count
			.map(merge_total(thread_total_notifications))
			.filter(&send_notification_count_filter),
	};

	// On quiet rounds (timeline empty) `thread_last_reads` is `Some`; emit
	// only threads whose read cursor advanced within the window. When the
	// timeline carried events `thread_last_reads` is `None`; emit all.
	// Initial sync (since == 0) is a full snapshot; bypass the gate so
	// clients with no prior cursor still see existing thread counts.
	let advanced_in_window = |root: &EventId| {
		initial
			|| thread_last_reads
				.is_none_or(|reads| reads.get(root).copied().is_some_and(&in_window))
	};

	let unread_thread_notifications = thread_counts
		.into_iter()
		.filter(|_| want_thread_unread)
		.filter(|(root, _)| advanced_in_window(root))
		.map(|(root, (notifications, highlights))| {
			let counts = UnreadNotificationsCount {
				notification_count: UInt::try_from(notifications).ok(),
				highlight_count: UInt::try_from(highlights).ok(),
			};

			(root, counts)
		})
		.collect();

	(unread_notifications, unread_thread_notifications)
}

#[tracing::instrument(
	name = "state",
	level = "trace",
	skip_all,
	fields(
	    full = %full_state,
	    after = ?state_after,
	    ss = ?since_shortstatehash,
	    hs = ?horizon_shortstatehash,
	    as = ?after_shortstatehash,
	    cs = %current_shortstatehash,
    )
)]
#[expect(clippy::too_many_arguments)]
async fn calculate_state_changes<'a>(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	full_state: bool,
	state_after: StateAfter,
	since_shortstatehash: Option<ShortStateHash>,
	horizon_shortstatehash: Option<ShortStateHash>,
	after_shortstatehash: Option<ShortStateHash>,
	current_shortstatehash: ShortStateHash,
	joined_since_last_sync: bool,
	witness: Option<&'a Witness>,
) -> Result<StateChanges> {
	let incremental = !full_state && !joined_since_last_sync && since_shortstatehash.is_some();

	// MSC4222: `state_after` requests need state at the *end* of the
	// timeline; legacy `state` requests need state at the *start*. Pick
	// the right delta endpoint, falling back to the room's current
	// shortstatehash when the preferred lookup is unavailable.
	let horizon_shortstatehash = state_after
		.requested()
		.then_some(after_shortstatehash)
		.unwrap_or(horizon_shortstatehash)
		.unwrap_or(current_shortstatehash);

	let since_shortstatehash = since_shortstatehash.unwrap_or(horizon_shortstatehash);

	let state_get_shorteventid = |user_id: &'a UserId| {
		services
			.state_accessor
			.state_get_shortid(
				horizon_shortstatehash,
				&StateEventType::RoomMember,
				user_id.as_str(),
			)
			.ok()
	};

	let lazy_state_ids = witness.map_async(|witness| {
		witness
			.iter()
			.stream()
			.ready_filter(|&user_id| user_id != sender_user)
			.broad_filter_map(|user_id| state_get_shorteventid(user_id))
			.into_future()
	});

	let state_diff_ids = incremental.then_async(|| {
		services
			.state_accessor
			.state_added((since_shortstatehash, horizon_shortstatehash))
			.boxed()
			.into_future()
	});

	let current_state_ids = (!incremental).then_async(|| {
		services
			.state_accessor
			.state_full_shortids(horizon_shortstatehash)
			.expect_ok()
			.boxed()
			.into_future()
	});

	// Full dump is strict; the delta relaxes the member filter under MSC4222.
	let after = state_after.requested();
	let state_events = current_state_ids
		.stream()
		.map(|ids| (false, ids))
		.chain(
			state_diff_ids
				.stream()
				.map(move |ids| (after, ids)),
		)
		.broad_filter_map(async |(after, (shortstatekey, shorteventid))| {
			lazy_filter(services, sender_user, witness, shortstatekey, shorteventid, after).await
		})
		.chain(lazy_state_ids.stream())
		.broad_filter_map(|shorteventid| {
			services
				.timeline
				.get_pdu_from_shorteventid(shorteventid)
				.ok()
		})
		.collect::<Vec<_>>()
		.await;

	let send_member_counts = state_events
		.iter()
		.any(|event| *event.kind() == RoomMember);

	let member_counts =
		send_member_counts.then_async(|| calculate_counts(services, room_id, sender_user));

	let (joined_member_count, invited_member_count, heroes) =
		member_counts.await.unwrap_or((None, None, None));

	Ok(StateChanges {
		heroes,
		joined_member_count,
		invited_member_count,
		state_events,
	})
}

async fn lazy_filter(
	services: &Services,
	sender_user: &UserId,
	witness: Option<&Witness>,
	shortstatekey: ShortStateKey,
	shorteventid: ShortEventId,
	after: bool,
) -> Option<ShortEventId> {
	let Some(witness) = witness else {
		return Some(shorteventid);
	};

	let (event_type, state_key) = services
		.short
		.get_statekey_from_short(shortstatekey)
		.await
		.ok()?;

	// An MSC4222 delta also keeps changed members the witness will not re-add
	// (lazy_state_ids covers witnessed ones), avoiding both a miss and a duplicate.
	let keep = event_type != StateEventType::RoomMember
		|| state_key == sender_user.as_str()
		|| (after && <&UserId>::try_from(state_key.as_str()).is_ok_and(|u| !witness.contains(u)));

	keep.then_some(shorteventid)
}

async fn calculate_counts(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
) -> (Option<u64>, Option<u64>, Option<Vec<OwnedUserId>>) {
	let joined_member_count = services
		.state_cache
		.room_joined_count(room_id)
		.unwrap_or(0);

	let invited_member_count = services
		.state_cache
		.room_invited_count(room_id)
		.unwrap_or(0);

	let (joined_member_count, invited_member_count) =
		join(joined_member_count, invited_member_count).await;

	let small_room = joined_member_count.saturating_add(invited_member_count) <= 5;

	let heroes = services
		.config
		.calculate_heroes
		.and_is(small_room)
		.then_async(|| calculate_heroes(services, room_id, sender_user));

	(Some(joined_member_count), Some(invited_member_count), heroes.await)
}

pub(crate) async fn calculate_heroes(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
) -> Vec<OwnedUserId> {
	const LIMIT: usize = 5;

	services
		.state_accessor
		.room_state_type_pdus(room_id, &StateEventType::RoomMember)
		.ready_filter_map(Result::ok)
		.filter_map(|pdu| filter_hero(services, room_id, sender_user, pdu))
		.take(LIMIT)
		.collect::<Vec<_>>()
		.await
}

async fn filter_hero<Pdu: Event>(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
	pdu: Pdu,
) -> Option<OwnedUserId> {
	let user_id = pdu.state_key().map(TryInto::try_into).flat_ok()?;

	if user_id == sender_user {
		return None;
	}

	let Ok(content): Result<RoomMemberEventContent, _> = pdu.get_content() else {
		return None;
	};

	// The membership was and still is invite or join
	if !matches!(content.membership, MembershipState::Join | MembershipState::Invite) {
		return None;
	}

	let (is_invited, is_joined) = join(
		services.state_cache.is_invited(user_id, room_id),
		services.state_cache.is_joined(user_id, room_id),
	)
	.await;

	if !is_joined && is_invited {
		return None;
	}

	Some(user_id.to_owned())
}

async fn typings_event_for_user(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
) -> Result<SyncEphemeralRoomEvent<TypingEventContent>> {
	Ok(SyncEphemeralRoomEvent {
		content: TypingEventContent {
			user_ids: services
				.typing
				.typing_users_for_user(room_id, sender_user)
				.await?,
		},
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn state_after_wraps_into_named_variant() {
		let events = StateEvents::default;

		assert!(matches!(StateAfter::Off.wrap(events()), RoomState::Before(_)));
		assert!(matches!(StateAfter::Stable.wrap(events()), RoomState::After(_)));
		assert!(matches!(StateAfter::Unstable.wrap(events()), RoomState::AfterUnstable(_)));

		assert!(!StateAfter::Off.requested());
		assert!(StateAfter::Stable.requested());
		assert!(StateAfter::Unstable.requested());
	}

	#[test]
	fn state_after_selects_unstable_when_both_opted_in() {
		// (use_state_after, use_state_after_unstable)
		assert!(matches!(StateAfter::from((false, false)), StateAfter::Off));
		assert!(matches!(StateAfter::from((true, false)), StateAfter::Stable));
		assert!(matches!(StateAfter::from((false, true)), StateAfter::Unstable));
		assert!(matches!(StateAfter::from((true, true)), StateAfter::Unstable));
	}
}
