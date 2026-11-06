use std::collections::{BTreeMap, HashSet};

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, join3, join4},
};
use ruma::{
	DeviceId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UInt, UserId,
	api::client::{
		filter::FilterDefinition,
		sync::sync_events::{
			UnreadNotificationsCount,
			v3::{Ephemeral, JoinedRoom, RoomAccountData, RoomSummary, StateEvents, Timeline},
		},
	},
	events::{
		AnyRawAccountDataEvent, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent,
		AnySyncStateEvent, StateEventType,
		TimelineEventType::*,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	serde::Raw,
};
use tuwunel_core::{
	Result, at, extract_variant, is_equal_to, pair_of, ref_at,
	utils::{
		BoolExt, IterStream, ReadyExt, TryFutureExtExt, math::ruma_from_u64, result::MapExpect,
		stream::WidebandExt,
	},
	warn,
};
use tuwunel_matrix::{
	Event,
	event::{Matches, trim_event_fields},
	pdu::{PduCount, PduEvent},
};
use tuwunel_service::{
	Services,
	rooms::{lazy_loading::Witness, read_receipt::PrivateReadEvents, short::ShortStateHash},
};

use self::notifications::{
	GateInputs, Gates as NotificationGates, assemble_threads as assemble_thread_notifications,
	assemble_unread as assemble_unread_notifications,
	compute_gates as compute_notification_gates, count_futures as notification_count_futures,
	in_window, send_thread_counts,
};
use super::{
	super::{load_timeline, strip_prev_state, timeline_prev_batch},
	StateAfter, StateChangeParams, StateChanges,
	state::{
		UserMetadata, calculate_state_changes, gather_room_metadata, gather_user_metadata,
		typings_event_for_user,
	},
};
use crate::{ignored_filter, is_empty_account_data_event, with_membership};

pub(super) mod notifications;

pub(super) struct RoomMetadata {
	pub(super) since_shortstatehash: Option<ShortStateHash>,
	pub(super) horizon_shortstatehash: Option<ShortStateHash>,
	pub(super) after_shortstatehash: Option<ShortStateHash>,
	pub(super) current_shortstatehash: Option<ShortStateHash>,
	pub(super) receipt_events: Vec<(OwnedUserId, Raw<AnySyncEphemeralRoomEvent>)>,
	pub(super) encrypted_room: Option<bool>,
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
	state_after: StateAfter,
	limited: bool,
	joined_since_last_sync: bool,
	prev_batch: Option<PduCount>,
}

struct FinalizeJoinedRoom<'a, S, F, W> {
	services: &'a Services,
	sender_user: &'a UserId,
	room_id: &'a RoomId,
	filter: &'a FilterDefinition,
	state_events: S,
	aggregates: JoinAggregates,
	receipt_events: Vec<(OwnedUserId, Raw<AnySyncEphemeralRoomEvent>)>,
	heroes: Option<Vec<OwnedUserId>>,
	joined_member_count: Option<u64>,
	invited_member_count: Option<u64>,
	thread_last_reads: Option<&'a BTreeMap<OwnedEventId, u64>>,
	send_notification_count_filter: F,
	flags: FinalizeJoinFlags,
	in_window: W,
	prev_batch: Option<PduCount>,
}

pub(super) type ThreadCounts = BTreeMap<OwnedEventId, (u64, u64)>;

struct JoinAggregates {
	room_events: Vec<PduEvent>,
	account_data_events: Vec<Raw<AnyRoomAccountDataEvent>>,
	typing_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
	private_read_events: Option<PrivateReadEvents>,
	notification_count: Option<UInt>,
	highlight_count: Option<UInt>,
	thread_counts: Option<ThreadCounts>,
	device_list_updates: HashSet<OwnedUserId>,
	left_encrypted_users: HashSet<OwnedUserId>,
}

struct FinalizeJoinFlags {
	encrypted: bool,
	full_state: bool,
	state_after: StateAfter,
	limited: bool,
	joined_since_last_sync: bool,
	initial: bool,
}

#[tracing::instrument(
	target = "tuwunel_api_client::sync::v3",
	name = "joined",
	level = "debug",
	skip_all,
	fields(
		room_id = ?room_id,
	),
)]
#[expect(clippy::too_many_arguments)]
pub(super) async fn load_room(
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
		timeline_changed || full_state || initial,
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
	)
	.boxed()
	.await;

	let (
		state_after,
		StateChanges {
			heroes,
			joined_member_count,
			invited_member_count,
			mut state_events,
			lazy_members,
		},
	) = compute_join_state_changes(
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

	let quiet_round = timeline_pdus.is_empty();

	let joined_sender_member = take_sender_membership_for_join(
		&mut state_events,
		sender_user,
		joined_since_last_sync,
		quiet_round,
		initial,
	);

	let prev_batch =
		compute_join_prev_batch(&timeline_pdus, joined_sender_member.as_ref(), since, next_batch);

	let in_window = in_window(since, next_batch);

	let NotificationGates {
		send_notification_counts,
		send_notification_count_filter,
	} = compute_notification_gates(
		GateInputs {
			last_notification_read,
			thread_last_reads: &thread_last_reads,
			quiet_round,
			since,
		},
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

	let state_events = state_events.into_iter().chain(lazy_members);

	let flags = FinalizeJoinFlags {
		encrypted,
		full_state,
		state_after,
		limited,
		joined_since_last_sync,
		initial,
	};

	let finalize = FinalizeJoinedRoom {
		services,
		sender_user,
		room_id,
		filter,
		state_events,
		aggregates,
		receipt_events,
		heroes,
		joined_member_count,
		invited_member_count,
		thread_last_reads: quiet_round.then_some(&thread_last_reads),
		send_notification_count_filter,
		flags,
		in_window,
		prev_batch,
	};

	let (joined_room, device_list_updates, left_encrypted_users) =
		finalize_joined_room(finalize).await;

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
) -> Result<(StateAfter, StateChanges)> {
	let Some(current_shortstatehash) = current_shortstatehash else {
		return Ok((state_after, StateChanges::default()));
	};

	let state_changes =
		calculate_state_changes(services, sender_user, room_id, StateChangeParams {
			full_state,
			state_after,
			since_shortstatehash,
			horizon_shortstatehash,
			after_shortstatehash,
			current_shortstatehash,
			joined_since_last_sync,
			witness,
			include_heroes: true,
		})
		.await;

	let incremental = !full_state && !joined_since_last_sync && since_shortstatehash.is_some();

	if !state_after.requested() || incremental {
		return state_changes.map(|state_changes| (state_after, state_changes));
	}

	match state_changes {
		| Ok(state_changes) => Ok((state_after, state_changes)),
		| Err(after_error) => {
			let after_boundary = after_shortstatehash.unwrap_or(current_shortstatehash);
			let legacy_boundary = horizon_shortstatehash.unwrap_or(current_shortstatehash);

			warn!(target: "tuwunel_api_client::sync::v3",
				%room_id,
				%after_boundary,
				?after_error,
				"Failed to load requested state-after boundary; retrying legacy state."
			);

			calculate_state_changes(services, sender_user, room_id, StateChangeParams {
				full_state,
				state_after: StateAfter::Off,
				since_shortstatehash,
				horizon_shortstatehash,
				after_shortstatehash,
				current_shortstatehash,
				joined_since_last_sync,
				witness,
				include_heroes: false,
			})
			.await
			.inspect_err(|legacy_error| {
				warn!(target: "tuwunel_api_client::sync::v3",
					%room_id,
					%after_boundary,
					%legacy_boundary,
					?after_error,
					?legacy_error,
					"Failed to load state-after and legacy state boundaries."
				);
			})
			.map(|state_changes| (StateAfter::Off, state_changes))
		},
	}
}

fn compute_join_prev_batch(
	timeline_pdus: &[(PduCount, PduEvent)],
	joined_sender_member: Option<&PduEvent>,
	since: u64,
	next_batch: u64,
) -> Option<PduCount> {
	timeline_prev_batch(timeline_pdus, PduCount::Normal(next_batch)).or_else(|| {
		joined_sender_member
			.is_some()
			.then_some(since)
			.map(Into::into)
	})
}

#[expect(clippy::too_many_arguments)]
async fn assemble_join_state_events(
	services: &Services,
	state_events: impl Iterator<Item = PduEvent> + Send,
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

async fn finalize_joined_room<S, F, W>(
	FinalizeJoinedRoom {
		services,
		sender_user,
		room_id,
		filter,
		state_events,
		aggregates,
		receipt_events,
		heroes,
		joined_member_count,
		invited_member_count,
		thread_last_reads,
		send_notification_count_filter,
		flags,
		in_window,
		prev_batch,
	}: FinalizeJoinedRoom<'_, S, F, W>,
) -> (JoinedRoom, HashSet<OwnedUserId>, HashSet<OwnedUserId>)
where
	S: Iterator<Item = PduEvent> + Send,
	F: Fn(&UInt) -> bool,
	W: Fn(u64) -> bool,
{
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

	let want_thread_unread = filter.room.timeline.unread_thread_notifications;

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

	let unread_notifications = assemble_unread_notifications(
		notification_count,
		highlight_count,
		thread_counts.as_ref(),
		send_notification_count_filter,
		want_thread_unread,
	);

	let args = BuildJoinedRoom {
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
		state_after,
		limited,
		joined_since_last_sync,
		prev_batch,
	};

	let joined_room = build_joined_room(args, filter.event_fields.as_deref());

	// Initial and busy rounds (no last-reads) send every thread's zero.
	let send_thread_zero = move |root: &OwnedEventId| {
		initial
			|| thread_last_reads
				.is_none_or(|reads| reads.get(root).copied().is_some_and(&in_window))
	};

	let send_threads = want_thread_unread
		&& send_thread_counts(
			!joined_room.is_empty(),
			thread_counts.iter().flat_map(ThreadCounts::keys),
			&send_thread_zero,
		);

	let thread_counts = send_threads
		.then_async(async move || match thread_counts {
			| Some(thread_counts) => thread_counts,
			| None =>
				services
					.pusher
					.thread_notification_counts(sender_user, room_id)
					.await,
		})
		.await;

	let unread_thread_notifications =
		assemble_thread_notifications(thread_counts, send_thread_zero);

	let joined_room = JoinedRoom {
		unread_thread_notifications,
		..joined_room
	};

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
		// The caller adds these once it knows the room is reported.
		unread_thread_notifications: BTreeMap::new(),
	}
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

async fn gather_device_list_updates(
	services: &Services,
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
		.collect()
		.await;

	let lists = (keys_changed, HashSet::new());

	state_events
		.iter()
		.filter(|_| !initial)
		.filter(|state_event| *state_event.event_type() == RoomMember)
		.filter_map(extract_membership)
		.chain(timeline_membership_changes)
		.fold(lists, |(mut dlu, mut leu): pair_of!(HashSet<_>), (membership, user_id)| {
			use MembershipState::*;

			match membership {
				| Join => dlu.insert(user_id),
				| Leave => leu.insert(user_id),
				| _ => false,
			};

			(dlu, leu)
		})
}

async fn assemble_state_events(
	services: &Services,
	state_events: impl Iterator<Item = PduEvent> + Send,
	sender_user: &UserId,
	encrypted: bool,
	include_in_state: impl Fn(&PduEvent) -> bool + Send + Sync,
	in_timeline: impl Fn(&PduEvent) -> bool + Send + Sync,
	event_fields: Option<&[String]>,
) -> Vec<Raw<AnySyncStateEvent>> {
	state_events
		.filter(include_in_state)
		.stream()
		.wide_then(|pdu| with_membership(services, pdu, sender_user, encrypted))
		.map(|pdu| strip_prev_state(pdu, sender_user, &in_timeline))
		.map(|pdu| trim_event_fields(pdu.into_format(), event_fields))
		.collect()
		.await
}
