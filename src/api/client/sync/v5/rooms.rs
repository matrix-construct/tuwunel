mod bump_stamp;
mod heroes;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet};

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, join3, join4},
};
use ruma::{
	JsOption, MxcUri, OwnedEventId, OwnedMxcUri, RoomId, UInt, UserId,
	api::client::sync::sync_events::{
		UnreadNotificationsCount,
		v5::{
			DisplayName,
			response::{Heroes, Room as ResponseRoom},
		},
	},
	events::{
		AnySyncStateEvent, StateEventType, TimelineEventType, room::member::MembershipState,
	},
	serde::Raw,
};
use tuwunel_core::{
	Error, Result, at, format_small_string, is_equal_to, ref_at,
	smallstr::SmallString,
	smallvec::SmallVec,
	utils::{
		BoolExt, IterStream, OptionExt, ReadyExt, TryFutureExtExt,
		hash::sha256::{
			Digest as Sha256Digest, delimited as sha256_delimited, hash as sha256_hash,
		},
		math::usize_from_ruma_bounded,
		result::{FlatOk, NotFound},
		stream::{BroadbandExt, WidebandExt},
	},
	warn,
};
use tuwunel_matrix::{
	Event, StateKey,
	pdu::{PduCount, PduEvent, RawPduId},
};
use tuwunel_service::{
	Services,
	rooms::short::ShortStateHash,
	sync::{RequiredState, Room, RoomConfig},
};

use self::{
	bump_stamp::{knock_bump_stamp, room_bump_stamp},
	heroes::calculate_heroes,
};
use super::{
	super::{load_timeline_fallible, stand_in_leave, strip_prev_state},
	Connection, ListIds, SyncInfo, WindowRoom,
};
use crate::{annotate_membership, ignored_filter, with_membership};

#[derive(Debug)]
pub(super) enum Failure {
	Timeline(Error),
	Payload(Error),
}

type ThreadCounts = BTreeMap<OwnedEventId, (u64, u64)>;
type EventTypeString = SmallString<[u8; 32]>;
type TimelineMembers<'a> = SmallVec<[&'a str; 2]>;
pub(super) type RoomDetails = (usize, HashSet<(StateEventType, StateKey)>);
pub(super) type Delivery = (ResponseRoom, Option<ShortStateHash>);
type CollectedState = (Vec<Raw<AnySyncStateEvent>>, Option<ShortStateHash>);
type Snapshots = (Option<ShortStateHash>, Option<(PduCount, ShortStateHash)>);

/// Most timeline events one room returns per response, as legacy sync allows.
const TIMELINE_LIMIT_MAX: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StateMode {
	Full,
	Delta(PduCount),
}

#[derive(Clone, Copy)]
struct StateSelection<'a> {
	mode: StateMode,
	previous: Option<&'a [u64]>,
	changed: bool,
	delivered: ShortStateHash,
}

/// What decides whether a delta sends one requested state event.
///
/// The event is sent when it is newer than the delta's base, when the room held
/// a different event at the base, or when it is a lazily loaded member.
#[derive(Clone, Copy, Debug, Default)]
struct Candidate {
	/// The event's timeline position, absent for an outlier.
	count: Option<PduCount>,

	/// Whether it was selected as a lazily loaded member.
	lazy: bool,

	/// Whether the room held the same event at the delta's base.
	same_at_since: bool,
}

#[tracing::instrument(
	name = "room",
	level = "debug",
	skip_all,
	fields(room_id, roomsince)
)]
pub(super) async fn handle_room(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window_room: &WindowRoom,
	room: &Room,
	config_changed: bool,
	room_details: RoomDetails,
) -> Result<Delivery, Failure> {
	let SyncInfo {
		services,
		sender_user,
		previous_connection_pos,
		direct_rooms,
		..
	} = sync_info;

	let WindowRoom { lists, membership, room_id, .. } = window_room;
	let roomsince = room.roomsince;

	if matches!(*membership, Some(MembershipState::Leave | MembershipState::Ban)) {
		return leave_or_ban_response(sync_info, conn, window_room, roomsince)
			.map_ok(|payload| (payload, None))
			.map_err(Failure::Payload)
			.await;
	}

	let is_invite = *membership == Some(MembershipState::Invite);

	let encrypted = services.state_accessor.is_encrypted_room(room_id);

	let (timeline_limit, required_state) = room_details;
	let allowed = membership_allows_required_state(membership.as_ref());

	let timeline = allowed.then_async(|| {
		load_timeline_fallible(
			services,
			sender_user,
			room_id,
			PduCount::Normal(roomsince),
			Some(PduCount::from(conn.next_batch)),
			timeline_limit,
		)
	});

	let timeline = timeline
		.map(Option::transpose)
		.map_err(Failure::Timeline);

	let (encrypted, timeline) = join(encrypted, timeline).await;

	// A failed load must fail the room, else roomsince advances past unsent events.
	let (timeline_pdus, limited, last_timeline_count) =
		timeline?.unwrap_or_else(|| (Vec::new(), false, PduCount::default()));

	let limited = room_timeline_limited(timeline_limit, limited);

	let prev_batch = timeline_pdus
		.first()
		.map(at!(0))
		.map(PduCount::into_unsigned)
		.as_ref()
		.map(ToString::to_string);

	let bump_stamp = match membership {
		| Some(MembershipState::Knock) => {
			// Redelivered rooms retain their own membership baseline even before roomsince.
			knock_bump_stamp(window_room.event_count, conn.next_batch)
		},
		| _ =>
			room_bump_stamp(
				services,
				sender_user,
				room_id,
				PduCount::Normal(roomsince),
				PduCount::from(conn.next_batch),
				last_timeline_count,
			)
			.map_err(Failure::Timeline)
			.await?,
	};

	let mode = state_mode(roomsince, room.required_state.is_empty());
	let state = StateSelection {
		mode,
		previous: config_changed.then_some(room.required_state.as_slice()),
		changed: allowed && state_may_have_changed(mode, last_timeline_count),
		delivered: room.shortstatehash,
	};

	let required_state = allowed
		.and_is(state.changed || state.previous.is_some())
		.then_some(required_state)
		.unwrap_or_default();

	let required_state = collect_required_state(
		services,
		sender_user,
		room_id,
		state,
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
		.wide_then(|(position, pdu)| {
			with_membership(services, pdu, sender_user, encrypted).map(move |pdu| (position, pdu))
		})
		.wide_then(|(position, pdu)| {
			services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
				.map(move |pdu| (position, pdu))
		})
		.map(|(position, pdu)| (position, Event::into_format(pdu)))
		.collect::<Vec<_>>();

	let meta = room_meta_future(services, room_id);
	let events = join3(timeline, required_state, invite_state);
	let member_counts = member_counts_future(services, room_id);
	let notification_counts = notification_counts_future(services, sender_user, room_id);
	let (
		(room_name, room_avatar),
		(timeline, (required_state, shortstatehash), invite_state),
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

	let previous_connection_pos = previous_connection_pos.filter(|_| !is_invite);
	let (initial, num_live) =
		room_timeline_metadata(roomsince, previous_connection_pos, &timeline);

	let timeline = timeline.into_iter().map(at!(1)).collect();
	let unread_notifications =
		merge_unread_notifications(highlight_count, notification_count, &thread_counts);

	let payload = ResponseRoom {
		initial,
		lists: lists.clone(),
		membership: membership.clone(),
		name: room_name.or(heroes_name),
		avatar: JsOption::from_option(room_avatar.or(heroes_avatar)),
		is_dm: direct_rooms.contains(room_id).then_some(true),
		heroes,
		required_state,
		invite_state: invite_state.flatten(),
		prev_batch: prev_batch.as_deref().map(Into::into),
		num_live,
		limited,
		timeline,
		bump_stamp,
		joined_count,
		invited_count,
		unread_notifications,
	};

	Ok((payload, shortstatehash))
}

async fn leave_or_ban_response(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	WindowRoom { lists, membership, room_id, .. }: &WindowRoom,
	roomsince: u64,
) -> Result<ResponseRoom> {
	// Clients read the departure from this event, not `membership`.
	let member_event = services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, sender_user.as_str())
		.map_ok(Event::into_format)
		.await
		.optional()
		.inspect_err(|error| warn!(%room_id, %error, "Unreadable own departure event"))
		.ok()
		.flatten()
		.or_else(|| {
			matches!(membership, Some(MembershipState::Leave))
				.then(|| stand_in_leave(services, room_id, sender_user))
				.and_then(Result::ok)
		});

	Ok(ResponseRoom {
		initial: roomsince.eq(&0).then_some(true),
		lists: lists.clone(),
		membership: membership.clone(),
		prev_batch: Some(conn.next_batch.to_string().into()),
		limited: true,
		required_state: member_event.into_iter().collect(),
		..Default::default()
	})
}

pub(super) fn merged_room_details(
	conn: &Connection,
	lists: &ListIds,
	room_id: &RoomId,
) -> RoomDetails {
	lists
		.iter()
		.filter_map(|list_id| conn.lists.get(list_id))
		.map(|list| &list.room_details)
		.chain(conn.subscriptions.get(room_id))
		.fold((0_usize, HashSet::new()), |mut details, config| {
			details.0 = details.0.max(usize_from_ruma_bounded(
				config.timeline_limit,
				TIMELINE_LIMIT_MAX,
				TIMELINE_LIMIT_MAX,
			));

			details
				.1
				.extend(config.required_state.iter().cloned());

			details
		})
}

pub(super) fn room_config((timeline_limit, required_state): &RoomDetails) -> RoomConfig {
	let timeline_limit = u64::try_from(*timeline_limit).expect("timeline limit must fit u64");
	let digest = sha256_hash(timeline_limit.to_be_bytes());

	required_state.iter().fold(
		(digest_word(digest), RequiredState::new()),
		|(hash, mut selectors), (event_type, state_key)| {
			let entry = required_state_hash(event_type, state_key.as_str());

			selectors.extend(state_key.as_str().ne("$LAZY").then_some(entry));

			(hash ^ entry, selectors)
		},
	)
}

fn state_mode(roomsince: u64, unknown: bool) -> StateMode {
	match (roomsince, unknown) {
		| (0, _) | (_, true) => StateMode::Full,
		| (roomsince, false) => StateMode::Delta(PduCount::Normal(roomsince)),
	}
}

fn required_state_hash(event_type: &StateEventType, state_key: &str) -> u64 {
	let event_type: EventTypeString = format_small_string!("{event_type}");
	let digest = sha256_delimited([event_type.as_str(), state_key].into_iter());

	digest_word(digest)
}

fn digest_word(digest: Sha256Digest) -> u64 {
	u64::from_be_bytes(
		digest[..8]
			.try_into()
			.expect("SHA-256 digest must contain eight bytes"),
	)
}

pub(super) fn membership_allows_required_state(membership: Option<&MembershipState>) -> bool {
	matches!(membership, None | Some(MembershipState::Join))
}

/// Whether a room's newest event lies past the delta cursor.
///
/// State changes arrive as timeline events, so a room with nothing newer than
/// the cursor has none to report. A full sync always reports.
fn state_may_have_changed(state_mode: StateMode, last_timeline_count: PduCount) -> bool {
	match state_mode {
		| StateMode::Full => true,
		| StateMode::Delta(since) => last_timeline_count > since,
	}
}

fn room_timeline_limited(timeline_limit: usize, limited: bool) -> bool {
	timeline_limit > 0 && limited
}

fn room_timeline_metadata<Event>(
	roomsince: u64,
	previous_connection_pos: Option<u64>,
	timeline_pdus: &[(PduCount, Event)],
) -> (Option<bool>, Option<UInt>) {
	let initial = roomsince.eq(&0).then_some(true);
	let num_live = previous_connection_pos
		.map(PduCount::from)
		.and_then(|previous_connection_pos| {
			timeline_pdus
				.iter()
				.rev()
				.map(|(position, _)| *position)
				.take_while(|position| *position > previous_connection_pos)
				.count()
				.try_into()
				.ok()
		});

	(initial, num_live)
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
	room_id: &'a RoomId,
) -> impl Future<Output = (Option<DisplayName>, Option<OwnedMxcUri>)> + Send + 'a {
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

	join(room_name, room_avatar)
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
	selection: StateSelection<'_>,
	required_state: &HashSet<(StateEventType, StateKey)>,
	timeline_pdus: &[(PduCount, PduEvent)],
	encrypted: bool,
) -> CollectedState {
	let StateSelection {
		mode: state_mode,
		previous,
		changed,
		delivered,
	} = selection;

	let lazy = required_state
		.iter()
		.any(is_equal_to!(&(StateEventType::RoomMember, "$LAZY".into())));

	let compared = changed
		&& required_state
			.iter()
			.any(|(_, state_key)| state_key != "$LAZY");

	let (current, since_state) = state_snapshots(services, room_id, selection, compared).await;

	// Equal hashes exclude changes, but newly requested keys may still be due.
	let state_unchanged = !changed
		|| since_state
			.is_some_and(|(_, since_shortstatehash)| current == Some(since_shortstatehash));

	// Keys skipped as unchanged vouch for no snapshot, so the room keeps its last one.
	let snapshot = current.filter(|_| !state_unchanged);

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

	let wildcard_state = required_state
		.iter()
		.filter(|(_, state_key)| (!state_unchanged || previous.is_some()) && state_key == "*")
		.stream()
		.flat_map(|(event_type, _)| {
			services
				.state_accessor
				.room_state_keys_with_ids(room_id, event_type)
				.ready_filter_map(Result::ok)
				.map(move |(state_key, event_id)| {
					((event_type.clone(), state_key), Some(event_id), false)
				})
		});

	let mut timeline_members: TimelineMembers<'_> = timeline_senders
		.chain(timeline_member_targets)
		.collect();

	timeline_members.sort_unstable();
	timeline_members.dedup();

	let timeline_members = timeline_members
		.into_iter()
		.map(|sender| (StateEventType::RoomMember, StateKey::from_str(sender)));

	let in_timeline = |event: &PduEvent| {
		timeline_pdus
			.iter()
			.map(ref_at!(1))
			.map(Event::event_id)
			.any(is_equal_to!(event.event_id()))
	};

	required_state
		.iter()
		.filter(|_| !state_unchanged || previous.is_some())
		.cloned()
		.map(|state| (state, None, false))
		.stream()
		.chain(wildcard_state)
		.chain(
			timeline_members
				.map(|state| (state, None, true))
				.stream(),
		)
		.broad_filter_map(async |(state, event_id, lazy)| {
			let (event_type, state_key) = state;
			let state_key: StateKey = match state_key.as_str() {
				| "$LAZY" | "*" => return None,
				| "$ME" => sender_user.as_str().into(),
				| _ => state_key,
			};

			let state_mode = previous
				.filter(|previous| {
					!state_was_requested(previous, &event_type, state_key.as_str(), sender_user)
				})
				.map(|_| StateMode::Full)
				.unwrap_or(state_mode);

			if state_unchanged && !lazy && state_mode != StateMode::Full {
				return None;
			}

			let event_id = match event_id {
				| Some(event_id) => event_id,
				| None =>
					services
						.state_accessor
						.room_state_get_id(room_id, &event_type, &state_key)
						.ok()
						.await?,
			};

			let pdu_id = services.timeline.get_pdu_id(&event_id).await.ok();
			let count = pdu_id.map(RawPduId::pdu_count);

			// Only a delivered snapshot vouches for an outlier; the cursor's snapshot
			// may already hold one the client was never sent.
			let same_at_since = since_state
				.filter(|(since, _)| {
					!lazy
						&& state_mode != StateMode::Full
						&& count.map_or(delivered != 0, |count| count <= *since)
				})
				.map_async(|(_, shortstatehash)| {
					services
						.state_accessor
						.state_get_id(shortstatehash, &event_type, &state_key)
						.ok()
				})
				.await
				.flatten()
				.is_some_and(|previous_event_id| previous_event_id == event_id);

			let pdu_id = state_is_required(state_mode, Candidate { count, lazy, same_at_since })
				.then_some(pdu_id)?;

			let mut pdu = match pdu_id {
				| None => services
					.timeline
					.get_outlier_pdu(&event_id)
					.await
					.ok()?,
				| Some(pdu_id) => services
					.timeline
					.get_pdu_from_id(&pdu_id)
					.or_else(|_| services.timeline.get_outlier_pdu(&event_id))
					.await
					.ok()?,
			};

			annotate_membership(services, &mut pdu, sender_user, encrypted).await;

			let pdu = strip_prev_state(pdu, sender_user, in_timeline);

			Some(Event::into_format(pdu))
		})
		.collect()
		.map(|events| (events, snapshot))
		.await
}

/// The room's current snapshot and the base a delta compares requested state with.
///
/// The current snapshot is read before any state key, so a change racing the
/// read is sent again rather than lost. The base is the snapshot the room's
/// required state was last delivered from, or without one, the snapshot before
/// the first event past the cursor.
async fn state_snapshots(
	services: &Services,
	room_id: &RoomId,
	StateSelection { mode, changed, delivered, .. }: StateSelection<'_>,
	compared: bool,
) -> Snapshots {
	let current = changed
		.then_async(|| {
			services
				.state
				.get_room_shortstatehash(room_id)
				.inspect_err(|error| warn!(%room_id, %error, "Unreadable room state snapshot"))
				.ok()
		})
		.await
		.flatten();

	// Falling back to current state would match every entry against itself.
	let since_state = match mode {
		| StateMode::Delta(since) if compared && delivered != 0 => Some((since, delivered)),
		| StateMode::Delta(since) if compared =>
			services
				.timeline
				.next_shortstatehash(room_id, since)
				.map_ok(|shortstatehash| (since, shortstatehash))
				.inspect_err(|error| warn!(%room_id, %error, "Unreadable state past the cursor"))
				.ok()
				.await,
		| _ => None,
	};

	(current, since_state)
}

fn state_was_requested(
	previous: &[u64],
	event_type: &StateEventType,
	state_key: &str,
	sender_user: &UserId,
) -> bool {
	let contains = |key| previous.contains(&required_state_hash(event_type, key));

	contains("*") || contains(state_key) || (state_key == sender_user.as_str() && contains("$ME"))
}

fn state_is_required(
	state_mode: StateMode,
	Candidate { count, lazy, same_at_since }: Candidate,
) -> bool {
	lazy || match state_mode {
		| StateMode::Full => true,
		| StateMode::Delta(since) => count.is_some_and(|count| count > since) || !same_at_since,
	}
}
