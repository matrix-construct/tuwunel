use std::{
	collections::{BTreeMap, HashMap, HashSet},
	time::Duration,
};

use axum::extract::State;
use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{OptionFuture, join, join3, join4, join5},
	pin_mut,
};
use ruma::{
	DeviceId, EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId,
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
		uiaa::UiaaResponse,
	},
	events::{
		AnyRawAccountDataEvent, AnySyncEphemeralRoomEvent, StateEventType,
		SyncEphemeralRoomEvent,
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
	Error, Result, at,
	debug::INFO_SPAN_LEVEL,
	err, error,
	error::inspect_debug_log,
	extract_variant, is_equal_to,
	matrix::{
		Event,
		event::Matches,
		pdu::{EventHash, PduCount, PduEvent},
	},
	pair_of, ref_at,
	result::FlatOk,
	trace,
	utils::{
		self, BoolExt, FutureBoolExt, IterStream, ReadyExt, TryFutureExtExt,
		future::{OptionStream, ReadyEqExt},
		math::ruma_from_u64,
		result::LogErr,
		stream::{BroadbandExt, Tools, TryExpect, WidebandExt},
	},
	warn,
};
use tuwunel_service::{
	Services,
	rooms::{
		lazy_loading,
		lazy_loading::{Options, Witness},
		short::{ShortEventId, ShortStateHash, ShortStateKey},
	},
};

use super::{load_timeline, share_encrypted_room};
use crate::{Ruma, RumaResponse, client::ignored_filter};

#[derive(Default)]
struct StateChanges {
	heroes: Option<Vec<OwnedUserId>>,
	joined_member_count: Option<u64>,
	invited_member_count: Option<u64>,
	state_events: Vec<PduEvent>,
}

type PresenceUpdates = HashMap<OwnedUserId, PresenceEventContent>;

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
    )
)]
pub(crate) async fn sync_events_route(
	State(services): State<crate::State>,
	body: Ruma<sync_events::v3::Request>,
) -> Result<sync_events::v3::Response, RumaResponse<UiaaResponse>> {
	let (sender_user, sender_device) = body.sender();

	services
		.presence
		.maybe_ping_presence(sender_user, &body.body.set_presence)
		.await
		.log_err()
		.ok();

	// Record user as actively syncing for push suppression heuristic.
	services.presence.note_sync(sender_user).await;

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
		let watchers = services.sync.watch(sender_user, sender_device);
		let next_batch = services.globals.wait_pending().await?;

		debug_assert!(since <= next_batch, "next_batch is monotonic");
		if since < next_batch || body.body.full_state {
			let response = build_sync_events(&services, &body, since, next_batch).await?;
			let empty = response.rooms.is_empty()
				&& response.presence.is_empty()
				&& response.account_data.is_empty()
				&& response.device_lists.is_empty()
				&& response.to_device.is_empty();

			if !empty || body.body.full_state {
				return Ok(response);
			}
		}

		// Wait for activity
		if time::timeout_at(stop_at, watchers).await.is_err() || services.server.is_stopping() {
			let response = build_empty_response(&services, &body, next_batch).await;
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
	body: &Ruma<sync_events::v3::Request>,
	next_batch: u64,
) -> sync_events::v3::Response {
	sync_events::v3::Response {
		device_one_time_keys_count: services
			.users
			.count_one_time_keys(body.sender_user(), body.sender_device())
			.await,

		..sync_events::v3::Response::new(next_batch.to_string())
	}
}

#[tracing::instrument(
	name = "build",
	level = INFO_SPAN_LEVEL,
	ret(level = "trace"),
	skip_all,
	fields(
		%since,
		%next_batch,
		count = ?services.globals.pending_count(),
    )
)]
async fn build_sync_events(
	services: &Services,
	body: &Ruma<sync_events::v3::Request>,
	since: u64,
	next_batch: u64,
) -> Result<sync_events::v3::Response> {
	let (sender_user, sender_device) = body.sender();

	let full_state = body.body.full_state;
	let filter = match body.body.filter.as_ref() {
		| None => FilterDefinition::default(),
		| Some(Filter::FilterDefinition(filter)) => filter.clone(),
		| Some(Filter::FilterId(filter_id)) => services
			.users
			.get_filter(sender_user, filter_id)
			.await
			.unwrap_or_default(),
	};

	let joined_rooms = services
		.state_cache
		.rooms_joined(sender_user)
		.ready_filter(|&room_id| filter.room.matches(room_id))
		.map(ToOwned::to_owned)
		.broad_filter_map(|room_id| {
			load_joined_room(
				services,
				sender_user,
				sender_device,
				room_id.clone(),
				since,
				next_batch,
				full_state,
				&filter,
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
		);

	let left_rooms = services
		.state_cache
		.rooms_left(sender_user)
		.ready_filter(|(room_id, _)| filter.room.matches(room_id))
		.broad_filter_map(|(room_id, _)| {
			handle_left_room(
				services,
				since,
				room_id.clone(),
				sender_user,
				next_batch,
				full_state,
				&filter,
			)
			.map_ok(move |left_room| (room_id, left_room))
			.ok()
		})
		.ready_filter_map(|(room_id, left_room)| left_room.map(|left_room| (room_id, left_room)))
		.collect();

	let invited_rooms = services
		.state_cache
		.rooms_invited(sender_user)
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
		});

	let knocked_rooms = services
		.state_cache
		.rooms_knocked(sender_user)
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
		});

	let presence_updates: OptionFuture<_> = services
		.config
		.allow_local_presence
		.then(|| process_presence_updates(services, since, next_batch, sender_user, &filter))
		.into();

	let account_data = services
		.account_data
		.changes_since(None, sender_user, since, Some(next_batch))
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Global))
		.collect();

	// Look for device list updates of this account
	let keys_changed = services
		.users
		.keys_changed(sender_user, since, Some(next_batch))
		.map(ToOwned::to_owned)
		.collect::<HashSet<_>>();

	let to_device_events = services
		.users
		.get_to_device_events(sender_user, sender_device, Some(since), Some(next_batch))
		.collect::<Vec<_>>();

	let device_one_time_keys_count = services
		.users
		.count_one_time_keys(sender_user, sender_device);

	// Remove all to-device events the device received *last time*
	let remove_to_device_events =
		services
			.users
			.remove_to_device_events(sender_user, sender_device, since);

	let (
		account_data,
		keys_changed,
		device_one_time_keys_count,
		((), to_device_events, presence_updates),
		(
			(joined_rooms, mut device_list_updates, left_encrypted_users),
			left_rooms,
			invited_rooms,
			knocked_rooms,
		),
	) = join5(
		account_data,
		keys_changed,
		device_one_time_keys_count,
		join3(remove_to_device_events, to_device_events, presence_updates),
		join4(joined_rooms, left_rooms, invited_rooms, knocked_rooms),
	)
	.boxed()
	.await;

	device_list_updates.extend(keys_changed);

	// If the user doesn't share an encrypted room with the target anymore, we need
	// to tell them
	let device_list_left = left_encrypted_users
		.into_iter()
		.stream()
		.broad_filter_map(async |user_id: OwnedUserId| {
			share_encrypted_room(services, sender_user, &user_id, None)
				.await
				.eq(&false)
				.then_some(user_id)
		})
		.collect()
		.await;

	let presence_events = presence_updates
		.into_iter()
		.flat_map(IntoIterator::into_iter)
		.map(|(sender, content)| PresenceEvent { content, sender })
		.map(|ref event| Raw::new(event))
		.filter_map(Result::ok)
		.collect();

	Ok(sync_events::v3::Response {
		account_data: GlobalAccountData { events: account_data },
		device_lists: DeviceLists {
			left: device_list_left,
			changed: device_list_updates.into_iter().collect(),
		},
		device_one_time_keys_count,
		// Fallback keys are not yet supported
		device_unused_fallback_key_types: None,
		next_batch: next_batch.to_string(),
		presence: Presence { events: presence_events },
		rooms: Rooms {
			leave: left_rooms,
			join: joined_rooms,
			invite: invited_rooms,
			knock: knocked_rooms,
		},
		to_device: ToDevice { events: to_device_events },
	})
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
#[allow(clippy::too_many_arguments)]
async fn handle_left_room(
	services: &Services,
	since: u64,
	ref room_id: OwnedRoomId,
	sender_user: &UserId,
	next_batch: u64,
	full_state: bool,
	filter: &FilterDefinition,
) -> Result<Option<LeftRoom>> {
	let left_count = services
		.state_cache
		.get_left_count(room_id, sender_user)
		.await
		.ok();

	let filter_exclude = filter
		.room
		.not_rooms
		.iter()
		.any(is_equal_to!(room_id));

	let filter_include = filter
		.room
		.rooms
		.as_ref()
		.is_some_and(|rooms| rooms.iter().any(is_equal_to!(room_id)));

	let too_soon = Some(next_batch) < left_count;
	let too_late = Some(since) >= left_count;
	let initial_sync = since == 0;
	let include_leave =
		filter.room.include_leave && !filter_exclude && (filter_include || initial_sync);

	// Left before last sync or after cutoff for next sync
	if (too_late && !include_leave) || too_soon {
		return Ok(None);
	}

	let is_not_found = services.metadata.exists(room_id).eq(&false);

	let is_disabled = services.metadata.is_disabled(room_id);

	let is_banned = services.metadata.is_banned(room_id);

	pin_mut!(is_not_found, is_disabled, is_banned);
	if is_not_found.or(is_disabled).or(is_banned).await {
		// This is just a rejected invite, not a room we know
		// Insert a leave event anyways for the client
		let event = PduEvent {
			event_id: EventId::new(services.globals.server_name()),
			sender: sender_user.to_owned(),
			origin: None,
			origin_server_ts: utils::millis_since_unix_epoch().try_into()?,
			kind: RoomMember,
			content: serde_json::from_str(r#"{"membership":"leave"}"#)?,
			state_key: Some(sender_user.as_str().into()),
			unsigned: None,
			// The following keys are dropped on conversion
			room_id: room_id.clone(),
			prev_events: Default::default(),
			auth_events: Default::default(),
			depth: uint!(1),
			redacts: None,
			hashes: EventHash::default(),
			signatures: None,
		};

		return Ok(Some(LeftRoom {
			account_data: RoomAccountData { events: Vec::new() },
			timeline: Timeline {
				limited: false,
				prev_batch: Some(next_batch.to_string()),
				events: Vec::new(),
			},
			state: RoomState::Before(StateEvents { events: vec![event.into_format()] }),
		}));
	}

	let mut left_state_events = Vec::new();

	let since_shortstatehash = services
		.timeline
		.prev_shortstatehash(room_id, PduCount::Normal(since).saturating_add(1))
		.await
		.ok();

	let since_state_ids: HashMap<_, OwnedEventId> = since_shortstatehash
		.map(|since_shortstatehash| {
			services
				.state_accessor
				.state_full_ids(since_shortstatehash)
		})
		.into_iter()
		.stream()
		.flatten()
		.collect()
		.await;

	let Ok(left_event_id): Result<OwnedEventId> = services
		.state_accessor
		.room_state_get_id(room_id, &StateEventType::RoomMember, sender_user.as_str())
		.await
	else {
		warn!("Left {room_id} but no left state event");
		return Ok(None);
	};

	let Ok(left_shortstatehash) = services
		.state
		.pdu_shortstatehash(&left_event_id)
		.await
	else {
		warn!(event_id = %left_event_id, "Leave event has no state in {room_id}");
		return Ok(None);
	};

	let mut left_state_ids: HashMap<_, _> = services
		.state_accessor
		.state_full_ids(left_shortstatehash)
		.collect()
		.await;

	let leave_shortstatekey = services
		.short
		.get_or_create_shortstatekey(&StateEventType::RoomMember, sender_user.as_str())
		.await;

	left_state_ids.insert(leave_shortstatekey, left_event_id);

	for (shortstatekey, event_id) in left_state_ids {
		if full_state || since_state_ids.get(&shortstatekey) != Some(&event_id) {
			let (event_type, state_key) = services
				.short
				.get_statekey_from_short(shortstatekey)
				.await?;

			if filter.room.state.lazy_load_options.is_enabled()
				&& event_type == StateEventType::RoomMember
				&& !full_state
				&& state_key
					.as_str()
					.try_into()
					.is_ok_and(|user_id: &UserId| sender_user != user_id)
			{
				continue;
			}

			let Ok(pdu) = services.timeline.get_pdu(&event_id).await else {
				error!("Pdu in state not found: {event_id}");
				continue;
			};

			left_state_events.push(pdu.into_format());
		}
	}

	Ok(Some(LeftRoom {
		account_data: RoomAccountData { events: Vec::new() },
		timeline: Timeline {
			// TODO: support left timeline events so we dont need to set limited to true
			limited: true,
			prev_batch: Some(next_batch.to_string()),
			events: Vec::new(), // and so we dont need to set this to empty vec
		},
		state: RoomState::Before(StateEvents { events: left_state_events }),
	}))
}

#[tracing::instrument(
	name = "joined",
	level = "debug",
	skip_all,
	fields(
		room_id = ?room_id,
	),
)]
#[allow(clippy::too_many_arguments)]
async fn load_joined_room(
	services: &Services,
	sender_user: &UserId,
	sender_device: &DeviceId,
	ref room_id: OwnedRoomId,
	since: u64,
	next_batch: u64,
	full_state: bool,
	filter: &FilterDefinition,
) -> Result<(JoinedRoom, HashSet<OwnedUserId>, HashSet<OwnedUserId>)> {
	let initial = since == 0;
	let timeline_limit: usize = filter
		.room
		.timeline
		.limit
		.unwrap_or_else(|| uint!(10))
		.try_into()?;

	let (timeline_pdus, limited, last_timeline_count) = load_timeline(
		services,
		sender_user,
		room_id,
		PduCount::Normal(since),
		Some(PduCount::Normal(next_batch)),
		timeline_limit,
	)
	.await?;

	let since_shortstatehash = services
		.timeline
		.prev_shortstatehash(room_id, PduCount::Normal(since).saturating_add(1))
		.ok();

	let horizon_shortstatehash: OptionFuture<_> = timeline_pdus
		.first()
		.map(at!(0))
		.map(|count| {
			services
				.timeline
				.get_shortstatehash(room_id, count)
				.inspect_err(inspect_debug_log)
		})
		.into();

	let current_shortstatehash = services
		.timeline
		.get_shortstatehash(room_id, last_timeline_count)
		.inspect_err(inspect_debug_log)
		.or_else(|_| services.state.get_room_shortstatehash(room_id))
		.map_err(|_| err!(Database(error!("Room {room_id} has no state"))));

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
		.collect::<HashMap<OwnedUserId, Raw<AnySyncEphemeralRoomEvent>>>();

	let encrypted_room = services.state_accessor.is_encrypted_room(room_id);

	let (
		(since_shortstatehash, horizon_shortstatehash, current_shortstatehash),
		receipt_events,
		encrypted_room,
	) = join3(
		join3(since_shortstatehash, horizon_shortstatehash, current_shortstatehash),
		receipt_events,
		encrypted_room,
	)
	.map(|((since, horizon, current), receipt, encrypted_room)| {
		Ok::<_, Error>(((since, horizon.flat_ok(), current?), receipt, encrypted_room))
	})
	.boxed()
	.await?;

	let lazy_load_options =
		[&filter.room.state.lazy_load_options, &filter.room.timeline.lazy_load_options];

	let lazy_loading_enabled = !encrypted_room
		&& lazy_load_options
			.iter()
			.any(|opts| opts.is_enabled());

	let lazy_loading_context = &lazy_loading::Context {
		user_id: sender_user,
		device_id: Some(sender_device),
		room_id,
		token: Some(since),
		options: Some(&filter.room.state.lazy_load_options),
	};

	// Reset lazy loading because this is an initial sync
	let lazy_load_reset: OptionFuture<_> = initial
		.then(|| services.lazy_loading.reset(lazy_loading_context))
		.into();

	lazy_load_reset.await;
	let witness: OptionFuture<_> = lazy_loading_enabled
		.then(|| {
			let witness: Witness = timeline_pdus
				.iter()
				.map(ref_at!(1))
				.map(Event::sender)
				.map(Into::into)
				.chain(receipt_events.keys().map(Into::into))
				.collect();

			services
				.lazy_loading
				.witness_retain(witness, lazy_loading_context)
		})
		.into();

	let sender_joined_count = services
		.state_cache
		.get_joined_count(room_id, sender_user);

	let since_encryption: OptionFuture<_> = since_shortstatehash
		.map(|shortstatehash| {
			services
				.state_accessor
				.state_get(shortstatehash, &StateEventType::RoomEncryption, "")
		})
		.into();

	let last_privateread_update = services
		.read_receipt
		.last_privateread_update(sender_user, room_id);

	let last_notification_read: OptionFuture<_> = timeline_pdus
		.is_empty()
		.then(|| {
			services
				.user
				.last_notification_read(sender_user, room_id)
		})
		.into();

	let (
		(last_privateread_update, last_notification_read),
		(sender_joined_count, since_encryption),
		witness,
	) = join3(
		join(last_privateread_update, last_notification_read),
		join(sender_joined_count, since_encryption),
		witness,
	)
	.await;

	let _encrypted_since_last_sync = !initial && encrypted_room && since_encryption.is_none();

	let joined_since_last_sync = sender_joined_count.is_ok_and(|count| count > since);

	let StateChanges {
		heroes,
		joined_member_count,
		invited_member_count,
		mut state_events,
	} = calculate_state_changes(
		services,
		sender_user,
		room_id,
		full_state || initial,
		since_shortstatehash,
		horizon_shortstatehash,
		current_shortstatehash,
		joined_since_last_sync,
		witness.as_ref(),
	)
	.await?;

	let is_sender_membership = |event: &PduEvent| {
		*event.event_type() == StateEventType::RoomMember.into()
			&& event
				.state_key()
				.is_some_and(is_equal_to!(sender_user.as_str()))
	};

	let joined_sender_member: Option<_> =
		(joined_since_last_sync && timeline_pdus.is_empty() && !initial)
			.then(|| {
				state_events
					.iter()
					.position(is_sender_membership)
					.map(|pos| state_events.swap_remove(pos))
			})
			.flatten();

	let prev_batch = timeline_pdus.first().map(at!(0)).or_else(|| {
		joined_sender_member
			.is_some()
			.then_some(since)
			.map(Into::into)
	});

	let send_notification_counts =
		last_notification_read.is_none_or(|last_count| last_count.gt(&since));

	let notification_count: OptionFuture<_> = send_notification_counts
		.then(|| {
			services
				.user
				.notification_count(sender_user, room_id)
				.map(TryInto::try_into)
				.unwrap_or(uint!(0))
		})
		.into();

	let highlight_count: OptionFuture<_> = send_notification_counts
		.then(|| {
			services
				.user
				.highlight_count(sender_user, room_id)
				.map(TryInto::try_into)
				.unwrap_or(uint!(0))
		})
		.into();

	let private_read_event: OptionFuture<_> = last_privateread_update
		.gt(&since)
		.then(|| {
			services
				.read_receipt
				.private_read_get(room_id, sender_user)
				.map(Result::ok)
		})
		.into();

	let typing_events = services
		.typing
		.last_typing_update(room_id)
		.and_then(async |count| {
			if count <= since {
				return Ok(Vec::<Raw<AnySyncEphemeralRoomEvent>>::new());
			}

			let typings = typings_event_for_user(services, room_id, sender_user).await?;

			Ok(vec![serde_json::from_str(&serde_json::to_string(&typings)?)?])
		})
		.unwrap_or(Vec::new());

	let keys_changed = services
		.users
		.room_keys_changed(room_id, since, Some(next_batch))
		.map(|(user_id, _)| user_id)
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>();

	let extract_membership = |event: &PduEvent| {
		let content: RoomMemberEventContent = event.get_content().ok()?;
		let user_id: OwnedUserId = event.state_key()?.parse().ok()?;

		Some((content.membership, user_id))
	};

	let timeline_membership_changes = timeline_pdus
		.iter()
		.filter(|_| !initial)
		.map(ref_at!(1))
		.filter_map(extract_membership)
		.collect::<Vec<_>>();

	let device_list_updates = state_events
		.iter()
		.stream()
		.ready_filter(|_| !initial)
		.ready_filter(|state_event| *state_event.event_type() == RoomMember)
		.ready_filter_map(extract_membership)
		.chain(timeline_membership_changes.stream())
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
		.then(async |(mut dlu, leu)| {
			dlu.extend(keys_changed.await);
			(dlu, leu)
		});

	let include_in_timeline = |event: &PduEvent| {
		let filter = &filter.room.timeline;
		filter.matches(event)
	};

	let room_events = timeline_pdus
		.into_iter()
		.stream()
		.wide_filter_map(|item| ignored_filter(services, item, sender_user))
		.map(at!(1))
		.chain(joined_sender_member.into_iter().stream())
		.ready_filter(include_in_timeline)
		.collect::<Vec<_>>();

	let account_data_events = services
		.account_data
		.changes_since(Some(room_id), sender_user, since, None)
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Room))
		.collect();

	let (
		(room_events, account_data_events),
		(typing_events, private_read_event),
		(notification_count, highlight_count),
		(device_list_updates, left_encrypted_users),
	) = join4(
		join(room_events, account_data_events),
		join(typing_events, private_read_event),
		join(notification_count, highlight_count),
		device_list_updates,
	)
	.boxed()
	.await;

	let is_in_timeline = |event: &PduEvent| {
		room_events
			.iter()
			.map(Event::event_id)
			.any(is_equal_to!(event.event_id()))
	};

	let include_in_state = |event: &PduEvent| {
		let filter = &filter.room.state;
		filter.matches(event) && (full_state || !is_in_timeline(event))
	};

	let state_events = state_events
		.into_iter()
		.filter(include_in_state)
		.map(Event::into_format)
		.collect();

	let heroes = heroes
		.into_iter()
		.flatten()
		.map(TryInto::try_into)
		.filter_map(Result::ok)
		.collect();

	let edus: Vec<Raw<AnySyncEphemeralRoomEvent>> = receipt_events
		.into_values()
		.chain(typing_events.into_iter())
		.chain(private_read_event.flatten().into_iter())
		.collect();

	let joined_room = JoinedRoom {
		account_data: RoomAccountData { events: account_data_events },
		ephemeral: Ephemeral { events: edus },
		state: RoomState::Before(StateEvents { events: state_events }),
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
				.map(Event::into_format)
				.collect(),
		},
		unread_notifications: UnreadNotificationsCount { highlight_count, notification_count },
		unread_thread_notifications: BTreeMap::new(),
	};

	Ok((joined_room, device_list_updates, left_encrypted_users))
}

#[tracing::instrument(
	name = "state",
	level = "trace",
	skip_all,
	fields(
	    full = %full_state,
	    ss = ?since_shortstatehash,
	    hs = ?horizon_shortstatehash,
	    cs = %current_shortstatehash,
    )
)]
#[allow(clippy::too_many_arguments)]
async fn calculate_state_changes<'a>(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	full_state: bool,
	since_shortstatehash: Option<ShortStateHash>,
	horizon_shortstatehash: Option<ShortStateHash>,
	current_shortstatehash: ShortStateHash,
	joined_since_last_sync: bool,
	witness: Option<&'a Witness>,
) -> Result<StateChanges> {
	let incremental = !full_state && !joined_since_last_sync && since_shortstatehash.is_some();

	let horizon_shortstatehash = horizon_shortstatehash.unwrap_or(current_shortstatehash);

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

	let lazy_state_ids: OptionFuture<_> = witness
		.map(|witness| {
			witness
				.iter()
				.stream()
				.ready_filter(|&user_id| user_id != sender_user)
				.broad_filter_map(|user_id| state_get_shorteventid(user_id))
				.into_future()
		})
		.into();

	let state_diff_ids: OptionFuture<_> = incremental
		.then(|| {
			services
				.state_accessor
				.state_added((since_shortstatehash, horizon_shortstatehash))
				.boxed()
				.into_future()
		})
		.into();

	let current_state_ids: OptionFuture<_> = (!incremental)
		.then(|| {
			services
				.state_accessor
				.state_full_shortids(horizon_shortstatehash)
				.expect_ok()
				.into_future()
		})
		.into();

	let state_events = current_state_ids
		.stream()
		.chain(state_diff_ids.stream())
		.broad_filter_map(async |(shortstatekey, shorteventid)| {
			lazy_filter(services, sender_user, witness, shortstatekey, shorteventid).await
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

	let member_counts: OptionFuture<_> = send_member_counts
		.then(|| calculate_counts(services, room_id, sender_user))
		.into();

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
) -> Option<ShortEventId> {
	if witness.is_none() {
		return Some(shorteventid);
	}

	let (event_type, state_key) = services
		.short
		.get_statekey_from_short(shortstatekey)
		.await
		.ok()?;

	(event_type != StateEventType::RoomMember || state_key == sender_user.as_str())
		.then_some(shorteventid)
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

	let heroes: OptionFuture<_> = small_room
		.then(|| calculate_heroes(services, room_id, sender_user))
		.into();

	(Some(joined_member_count), Some(invited_member_count), heroes.await)
}

async fn calculate_heroes(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
) -> Vec<OwnedUserId> {
	services
		.timeline
		.all_pdus(sender_user, room_id)
		.ready_filter(|(_, pdu)| pdu.kind == RoomMember)
		.fold_default(|heroes: Vec<_>, (_, pdu)| {
			fold_hero(heroes, services, room_id, sender_user, pdu)
		})
		.await
}

async fn fold_hero(
	mut heroes: Vec<OwnedUserId>,
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
	pdu: PduEvent,
) -> Vec<OwnedUserId> {
	let Some(user_id): Option<&UserId> = pdu
		.state_key
		.as_deref()
		.map(TryInto::try_into)
		.flat_ok()
	else {
		return heroes;
	};

	if user_id == sender_user {
		return heroes;
	}

	let Ok(content): Result<RoomMemberEventContent, _> = pdu.get_content() else {
		return heroes;
	};

	// The membership was and still is invite or join
	if !matches!(content.membership, MembershipState::Join | MembershipState::Invite) {
		return heroes;
	}

	if heroes.iter().any(is_equal_to!(user_id)) {
		return heroes;
	}

	let (is_invited, is_joined) = join(
		services.state_cache.is_invited(user_id, room_id),
		services.state_cache.is_joined(user_id, room_id),
	)
	.await;

	if !is_joined && is_invited {
		return heroes;
	}

	heroes.push(user_id.to_owned());
	heroes
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
