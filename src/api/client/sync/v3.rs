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
	DeviceId, EventId, OwnedRoomId, OwnedUserId, RoomId, UInt, UserId,
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
	Result, at,
	debug::INFO_SPAN_LEVEL,
	debug_error, err,
	error::{inspect_debug_log, inspect_log},
	extract_variant, is_equal_to, is_false, is_true,
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
		future::{OptionStream, ReadyBoolExt},
		math::ruma_from_u64,
		option::OptionExt,
		result::MapExpect,
		stream::{BroadbandExt, Tools, TryExpect, WidebandExt},
		string::to_small_string,
	},
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
use crate::{Ruma, client::ignored_filter};

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
		device_id = %body.sender_device.as_deref().map_or("<no device>", |x| x.as_str()),
    )
)]
pub(crate) async fn sync_events_route(
	State(services): State<crate::State>,
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
	let ping_presence = services
		.presence
		.maybe_ping_presence(sender_user, body.sender_device.as_deref(), set_presence)
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
			.watch(sender_user, sender_device, watch_rooms);

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
	sync_events::v3::Response {
		device_one_time_keys_count: sender_device
			.map_async(|sender_device| {
				services
					.users
					.count_one_time_keys(sender_user, sender_device)
			})
			.await
			.unwrap_or_default(),

		..sync_events::v3::Response::new(to_small_string(next_batch))
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
async fn build_sync_events(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	since: u64,
	next_batch: u64,
	full_state: bool,
	filter: &FilterDefinition,
) -> Result<sync_events::v3::Response> {
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
		);

	let left_rooms = services
		.state_cache
		.rooms_left_state(sender_user)
		.ready_filter(|(room_id, _)| filter.room.matches(room_id))
		.broad_filter_map(|(room_id, _)| {
			handle_left_room(
				services,
				since,
				room_id.clone(),
				sender_user,
				next_batch,
				full_state,
				filter,
			)
			.map_ok(move |left_room| (room_id, left_room))
			.ok()
		})
		.ready_filter_map(|(room_id, left_room)| left_room.map(|left_room| (room_id, left_room)))
		.collect();

	let invited_rooms = services
		.state_cache
		.rooms_invited_state(sender_user)
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
		});

	let presence_updates = services
		.config
		.allow_local_presence
		.then_async(|| {
			process_presence_updates(services, since, next_batch, sender_user, filter)
		});

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
		(_, to_device_events, device_one_time_keys_count),
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
		join3(remove_to_device_events, to_device_events, device_one_time_keys_count),
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
		device_one_time_keys_count: device_one_time_keys_count.unwrap_or_default(),
		// Fallback keys are not yet supported
		device_unused_fallback_key_types: None,
		next_batch: to_small_string(next_batch),
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
			event_id: EventId::new(services.globals.server_name()),
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
			signatures: None,
		};

		return Ok(Some(LeftRoom {
			account_data: RoomAccountData::default(),
			state: RoomState::Before(StateEvents { events: vec![event.into_format()] }),
			timeline: Timeline {
				limited: false,
				events: Default::default(),
				prev_batch: Some(left_count.to_string()),
			},
		}));
	}

	load_left_room(services, sender_user, room_id, since, left_count, full_state, filter).await
}

#[tracing::instrument(name = "load", level = "debug", skip_all)]
async fn load_left_room(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	since: u64,
	left_count: u64,
	full_state: bool,
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

	let left_shortstatehash = services
		.timeline
		.get_shortstatehash(room_id, PduCount::Normal(left_count))
		.inspect_err(inspect_debug_log)
		.or_else(|_| services.state.get_room_shortstatehash(room_id))
		.map_err(|_| err!(Database(error!("Room {room_id} has no state"))));

	let (since_shortstatehash, horizon_shortstatehash, left_shortstatehash) =
		join3(since_shortstatehash, horizon_shortstatehash, left_shortstatehash)
			.boxed()
			.await;

	let StateChanges { state_events, .. } = calculate_state_changes(
		services,
		sender_user,
		room_id,
		full_state || initial,
		since_shortstatehash,
		horizon_shortstatehash.flatten(),
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

	let state_events = state_events
		.into_iter()
		.filter(|pdu| filter.room.state.matches(pdu))
		.filter(|pdu| timeline_limit > 0 || !is_sender_membership(pdu))
		.chain(timeline_sender_member)
		.map(Event::into_format)
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
		.collect::<Vec<_>>();

	let account_data_events = services
		.account_data
		.changes_since(Some(room_id), sender_user, since, None)
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Room))
		.collect();

	let (account_data_events, timeline_events) = join(account_data_events, timeline_events)
		.boxed()
		.await;

	Ok(Some(LeftRoom {
		account_data: RoomAccountData { events: account_data_events },
		state: RoomState::Before(StateEvents { events: state_events }),
		timeline: Timeline {
			prev_batch,
			limited: limited || timeline_limit == 0,
			events: timeline_events
				.into_iter()
				.map(Event::into_format)
				.collect(),
		},
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
#[expect(clippy::too_many_arguments)]
async fn load_joined_room(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
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
		.map(TryInto::try_into)
		.map_expect("UInt to usize")
		.unwrap_or(10)
		.min(100);

	let (timeline_pdus, limited, last_timeline_count) = load_timeline(
		services,
		sender_user,
		room_id,
		PduCount::Normal(since),
		Some(PduCount::Normal(next_batch)),
		timeline_limit,
	)
	.await?;

	let timeline_changed = last_timeline_count.into_unsigned() > since;
	debug_assert!(
		timeline_pdus.is_empty() || timeline_changed,
		"if timeline events, last_timeline_count must be in the since window."
	);

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
		.collect::<HashMap<OwnedUserId, Raw<AnySyncEphemeralRoomEvent>>>();

	let (
		(since_shortstatehash, horizon_shortstatehash, current_shortstatehash),
		receipt_events,
		encrypted_room,
	) = join3(
		join3(since_shortstatehash, horizon_shortstatehash, current_shortstatehash),
		receipt_events,
		encrypted_room,
	)
	.map(|((since, horizon, current), receipt, encrypted_room)| -> Result<_> {
		Ok((
			(since.flatten(), horizon.flat_ok(), current.transpose()?),
			receipt,
			encrypted_room,
		))
	})
	.boxed()
	.await?;

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
			.chain(receipt_events.keys().map(Into::into))
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

	let last_privateread_update = services
		.read_receipt
		.last_privateread_update(sender_user, room_id);

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

	let _encrypted_since_last_sync =
		!initial && encrypted_room.is_some_and(is_true!()) && since_encryption.is_none();

	let joined_since_last_sync = sender_joined_count.unwrap_or(0) > since;

	let state_changes = current_shortstatehash.map_async(|current_shortstatehash| {
		calculate_state_changes(
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
	});

	let StateChanges {
		heroes,
		joined_member_count,
		invited_member_count,
		mut state_events,
	} = state_changes
		.await
		.transpose()?
		.unwrap_or_default();

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

	let send_notification_counts = last_notification_read
		.flatten()
		.is_none_or(|last_count| last_count > since && last_count <= next_batch);

	let send_notification_resets = last_notification_read
		.flatten()
		.is_some_and(|last_count| last_count > since);

	let send_notification_count_filter =
		|count: &UInt| *count != uint!(0) || send_notification_resets;

	let notification_count = send_notification_counts.then_async(|| {
		services
			.pusher
			.notification_count(sender_user, room_id)
			.map(TryInto::try_into)
			.unwrap_or(uint!(0))
	});

	let highlight_count = send_notification_counts.then_async(|| {
		services
			.pusher
			.highlight_count(sender_user, room_id)
			.map(TryInto::try_into)
			.unwrap_or(uint!(0))
	});

	let private_read_event = last_privateread_update.gt(&since).then_async(|| {
		services
			.read_receipt
			.private_read_get(room_id, sender_user)
			.map(Result::ok)
	});

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
		unread_notifications: UnreadNotificationsCount {
			highlight_count: highlight_count.filter(send_notification_count_filter),
			notification_count: notification_count.filter(send_notification_count_filter),
		},
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
#[expect(clippy::too_many_arguments)]
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

	let heroes = services
		.config
		.calculate_heroes
		.and_is(small_room)
		.then_async(|| calculate_heroes(services, room_id, sender_user));

	(Some(joined_member_count), Some(invited_member_count), heroes.await)
}

async fn calculate_heroes(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
) -> Vec<OwnedUserId> {
	services
		.state_accessor
		.room_state_type_pdus(room_id, &StateEventType::RoomMember)
		.ready_filter_map(Result::ok)
		.fold_default(|heroes: Vec<_>, pdu| {
			fold_hero(heroes, services, room_id, sender_user, pdu)
		})
		.await
}

async fn fold_hero<Pdu: Event>(
	mut heroes: Vec<OwnedUserId>,
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
	pdu: Pdu,
) -> Vec<OwnedUserId> {
	let Some(user_id): Option<&UserId> = pdu.state_key().map(TryInto::try_into).flat_ok() else {
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
