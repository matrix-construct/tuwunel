use std::collections::BTreeMap;

use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt,
	future::{join, join3},
};
use itertools::Either;
use ruma::{
	OwnedEventId, OwnedRoomId, RoomId, UserId,
	api::client::{
		filter::FilterDefinition,
		sync::sync_events::v3::{LeftRoom, RoomAccountData, StateEvents, Timeline},
	},
	events::{
		AnyRawAccountDataEvent, StateEventType,
		TimelineEventType::*,
		room::{history_visibility::HistoryVisibility, member::MembershipState},
	},
};
use tuwunel_core::{
	Error, Result, at, debug,
	error::inspect_debug_log,
	extract_variant, is_equal_to, ref_at,
	smallvec::SmallVec,
	utils::{
		BoolExt, FutureBoolExt, IterStream, ReadyExt, TryFutureExtExt,
		future::{ReadyBoolExt, ReadyEqExt},
		option::OptionExt,
		result::{MapExpect, NotFound},
		stream::{BroadbandExt, WidebandExt},
	},
	warn,
};
use tuwunel_matrix::{
	Event,
	event::{Matches, trim_event_fields},
	pdu::{PduCount, PduEvent},
};
use tuwunel_service::{Services, rooms::short::ShortStateHash};

use super::{
	super::{
		TimelineQuery, TimelineWindow, is_departure, load_timeline, own_departure,
		stand_in_leave, strip_state_unsigned, timeline_prev_batch,
	},
	StateAfter, StateChangeParams, missing_room_state,
	state::calculate_state_changes,
};
use crate::{ignored_filter, is_empty_account_data_event, with_membership};

type TimelineEventIds = SmallVec<[OwnedEventId; 1]>;

/// Carries the requester's departure and the snapshots its state is judged by.
///
/// `after` is the snapshot `state_after` is served from and `left` the one the
/// legacy field falls back to. For a user who never joined, both refer to the
/// snapshot recorded before the departure event, and the caller applies that
/// event.
struct Departure {
	once_joined: bool,
	event: Option<PduEvent>,
	after: Option<ShortStateHash>,
	left: Option<ShortStateHash>,
}

struct LoadParams<'a> {
	sender_user: &'a UserId,
	room_id: &'a RoomId,
	since: u64,
	horizon: u64,
	snapshot: Option<ShortStateHash>,
	full_state: bool,
	state_after: StateAfter,
	filter: &'a FilterDefinition,
	departure: Option<PduEvent>,
}

struct ServedState {
	sees_room_state: bool,
	recorded: Option<ShortStateHash>,
	full_state: bool,
	state_after: StateAfter,
	since: Option<ShortStateHash>,
	horizon: Option<ShortStateHash>,
	after: Option<ShortStateHash>,
}

struct TimelineParams<'a> {
	sender_user: &'a UserId,
	filter: &'a FilterDefinition,
	limit: usize,
	encrypted: bool,
}

pub(super) fn collect_rooms<'a>(
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
		.rooms_departed(sender_user)
		.ready_filter(|room_id| filter.room.matches(room_id))
		.map(ToOwned::to_owned)
		.broad_filter_map(move |room_id| {
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

#[tracing::instrument(
	target = "tuwunel_api_client::sync::v3",
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
		.get_departure_count(room_id, sender_user)
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

	let forgotten = services
		.state_cache
		.is_forgotten(sender_user, room_id)
		.await?;

	if forgotten && since == 0 {
		return Ok(None);
	}

	let is_not_found = services.metadata.exists(room_id).is_false();

	let is_disabled = services.metadata.is_disabled(room_id);

	let is_banned = services.metadata.is_banned(room_id);

	let refused = forgotten || is_not_found.or2(is_disabled, is_banned).await;

	// Read once: admission judges it, and a never-member's state is anchored on it.
	let needs_departure = forgotten || !refused;
	let departure = needs_departure
		.then_async(|| own_departure(services, room_id, sender_user))
		.map(Option::flatten)
		.await;

	let notice_only = refused || {
		let membership = departure
			.as_ref()
			.and_then(|event| event.membership_for(sender_user));

		departed_out_of_band(services, room_id, membership, since).await
	};

	let retained = async {
		if notice_only {
			return Ok(None);
		}

		let Some(count) = services
			.state_accessor
			.departure_count(sender_user, room_id)
			.await?
		else {
			return Ok(None);
		};

		services
			.state_accessor
			.departure_shortstatehash(room_id, count)
			.map_ok(|hash| Some((count, hash)))
			.await
	}
	.await
	.inspect_err(|error| warn!(target: "tuwunel_api_client::sync::v3", %room_id, %sender_user, %error, "Unreadable departure snapshot"));

	// The notification position isn't a safe history bound on a failed read.
	if notice_only || retained.is_err() {
		let departure = departure
			.filter(|_| forgotten || retained.is_err())
			.map(Event::into_format);

		let event =
			departure.map_or_else(|| stand_in_leave(services, room_id, sender_user), Ok)?;

		let state = state_after.wrap(StateEvents {
			events: vec![trim_event_fields(event, filter.event_fields.as_deref())],
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

	let (horizon, snapshot) =
		retained?.map_or((left_count, None), |(count, hash)| (count.into_unsigned(), Some(hash)));

	load_left_room(services, LoadParams {
		sender_user,
		room_id,
		since,
		horizon,
		snapshot,
		full_state,
		state_after,
		filter,
		departure,
	})
	.await
}

/// Whether the room holds no departure event this sync can send.
///
/// An invite rescinded or rejected after this server left the room writes only
/// the membership row. The state then holds no departure for the user, or an
/// older one with no event after the client's position to diff it against, and
/// either way the rest of the room's frozen state is not theirs to receive. A
/// ban stays with the ordinary loader, which serves it whatever the row's count
/// says, and so does any departure when the read of the room's last event
/// fails, which is logged and proves nothing.
async fn departed_out_of_band(
	services: &Services,
	room_id: &RoomId,
	membership: Option<MembershipState>,
	since: u64,
) -> bool {
	let Some(membership) = membership else {
		return true;
	};

	membership != MembershipState::Ban
		&& since != 0
		&& services
			.timeline
			.last_timeline_count(None, room_id, None)
			.inspect_err(|error| warn!(target: "tuwunel_api_client::sync::v3", %room_id, %since, %error, "Unreadable last event count"))
			.map(|last| last.is_ok_and(|last| last <= PduCount::Normal(since)))
			.await
}

#[tracing::instrument(
	target = "tuwunel_api_client::sync::v3",
	name = "load",
	level = "debug",
	skip_all
)]
async fn load_left_room(
	services: &Services,
	LoadParams {
		sender_user,
		room_id,
		since,
		horizon,
		snapshot,
		full_state,
		state_after,
		filter,
		departure,
	}: LoadParams<'_>,
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

	let query = TimelineQuery {
		sender_user,
		room_id,
		since: PduCount::Normal(since),
		next_batch: Some(PduCount::Normal(horizon)),
		limit: timeline_limit.max(1),
	};

	let TimelineWindow { pdus: timeline_pdus, limited, resume, .. } =
		load_timeline(services, query)
			.await
			.unwrap_or_default();

	let first = timeline_pdus.first().map(at!(0));
	let snapshots = window_snapshots(services, room_id, since, horizon, first);

	let after_requested = state_after.requested();
	let departure = resolve_departure(services, room_id, snapshot, departure);

	let ((since_shortstatehash, horizon_shortstatehash), departure) = join(snapshots, departure)
		.boxed() // erase region
		.await;

	let Departure {
		once_joined,
		event: departure,
		after: after_shortstatehash,
		left: left_shortstatehash,
	} = departure;

	// A never-member's legacy state starts no later than their departure; a
	// timeline reaching past it leaves the departure's own snapshot to serve.
	let starts_in_time = once_joined
		|| horizon_shortstatehash.is_none()
		|| starts_by_departure(services, room_id, first, departure.as_ref()).await;

	let horizon_shortstatehash = horizon_shortstatehash.filter(|_| starts_in_time);

	let recorded_shortstatehash = state_after
		.snapshot(after_shortstatehash, horizon_shortstatehash)
		.or(left_shortstatehash);

	let sees_room_state =
		may_see_room_state(services, once_joined, recorded_shortstatehash).await;

	let state_events = served_state(services, sender_user, room_id, ServedState {
		sees_room_state,
		recorded: recorded_shortstatehash,
		full_state: full_state || initial,
		state_after,
		since: since_shortstatehash,
		horizon: horizon_shortstatehash,
		after: after_shortstatehash,
	})
	.await?;

	let is_sender_membership = |event: &PduEvent| {
		*event.kind() == RoomMember && event.state_key() == Some(sender_user.as_str())
	};

	let in_timeline = in_timeline(&timeline_pdus);

	// Room history stays capped at the horizon, while the requester's current
	// membership remains the last membership update the client receives: append it
	// past a non-empty capped timeline, or deliver it in state otherwise.
	let trailing_departure = departure
		.as_ref()
		.filter(|departure| {
			once_joined
				&& timeline_limit > 0
				&& !timeline_pdus.is_empty()
				&& !in_timeline(departure)
				&& filter.room.timeline.matches(departure)
		})
		.cloned();

	let has_trailing = trailing_departure.is_some();
	let rejected_after = !once_joined && after_requested;
	let departure = departure.filter(|departure| {
		after_requested
			|| (!has_trailing
				&& (timeline_limit == 0
					|| !in_timeline(departure)
					|| !filter.room.timeline.matches(departure)))
	});

	let has_departure = departure.is_some();

	// With no departure to apply, a never-member's invite or knock from before the
	// rejection is not served as their state.
	let superseded = |pdu: &PduEvent| {
		(has_departure || rejected_after)
			&& is_sender_membership(pdu)
			&& (has_departure || !is_departure(pdu, sender_user))
	};

	// A zero limit moves only the requester's own membership into state, and only
	// the departure itself when there is one.
	let timeline_sender_member = timeline_limit
		.eq(&0)
		.then(|| timeline_pdus.last().map(ref_at!(1)))
		.flatten()
		.filter(|pdu| is_sender_membership(pdu))
		.filter(|pdu| {
			departure
				.as_ref()
				.map(|departure| departure.event_id() == pdu.event_id())
				.unwrap_or_else(|| !superseded(pdu))
		})
		.cloned();

	let transfers_member = timeline_sender_member.is_some();

	// The transfer then carries the departure itself past the state filter.
	let departure = departure.filter(|_| !transfers_member);

	let encrypted = services
		.state_accessor
		.is_encrypted_room(room_id)
		.await;

	let event_fields = filter.event_fields.as_deref();

	let state_events = state_events
		.filter(|pdu| !superseded(pdu))
		.chain(departure)
		.filter(|pdu| filter.room.state.matches(pdu))
		.filter(|pdu| !transfers_member || has_departure || !is_sender_membership(pdu))
		.chain(timeline_sender_member)
		.filter(visible_state(sender_user, sees_room_state))
		.stream()
		.wide_then(|pdu| with_membership(services, pdu, sender_user, encrypted))
		.map(|pdu| strip_state_unsigned(pdu, sender_user, &in_timeline))
		.map(|pdu| trim_event_fields(pdu.into_format(), event_fields))
		.collect();

	let left_prev_batch = timeline_limit
		.eq(&0)
		.then_some(horizon)
		.map(PduCount::Normal);

	let prev_batch = timeline_prev_batch(&timeline_pdus, PduCount::Normal(horizon), resume)
		.filter(|_| timeline_limit > 0)
		.or(left_prev_batch)
		.as_ref()
		.map(ToString::to_string);

	let timeline_events =
		timeline_events(services, timeline_pdus, trailing_departure, TimelineParams {
			sender_user,
			filter,
			limit: timeline_limit,
			encrypted,
		})
		.map(|pdu| trim_event_fields(pdu.into_format(), event_fields))
		.collect();

	let account_data_events = services
		.account_data
		.changes_since(Some(room_id), sender_user, since, None)
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Room))
		.ready_filter(move |e| since != 0 || !is_empty_account_data_event(e))
		.collect();

	let (state_events, account_data_events, timeline_events) =
		join3(state_events, account_data_events, timeline_events).await;

	let state = state_after.wrap(StateEvents { events: state_events });

	Ok(Some(LeftRoom {
		account_data: RoomAccountData { events: account_data_events },
		state,
		timeline: Timeline {
			prev_batch,
			limited: limited || timeline_limit == 0 || has_trailing,
			events: timeline_events,
		},
	}))
}

/// Reads the snapshots at the client's position and at the timeline's start.
///
/// The first is read only when the client's position precedes the departure,
/// and the second only when the timeline has a first event.
async fn window_snapshots(
	services: &Services,
	room_id: &RoomId,
	since: u64,
	horizon: u64,
	first: Option<PduCount>,
) -> (Option<ShortStateHash>, Option<ShortStateHash>) {
	let since_shortstatehash = since
		.lt(&horizon)
		.then_async(|| {
			services
				.timeline
				.next_shortstatehash(room_id, PduCount::Normal(since))
				.ok()
		})
		.map(Option::flatten);

	let first_shortstatehash = first
		.map_async(|count| {
			services
				.timeline
				.get_shortstatehash(room_id, count)
				.inspect_err(log_horizon_error)
				.ok()
		})
		.map(Option::flatten);

	join(since_shortstatehash, first_shortstatehash).await
}

/// Resolves the requester's departure and the snapshots tied to it.
///
/// Room history stays capped at the departure horizon, while the requester's
/// current membership event stays the last membership update the client
/// receives. For a former member, the caller supplies the authoritative
/// departure snapshot for both fields and keeps the current membership event
/// so the response can append it past a capped timeline or deliver it in state.
/// For a user who never joined, the caller supplies the departure it verified
/// in current state, and both fields use that event's snapshot regardless of
/// any recorded count.
#[tracing::instrument(level = "trace", skip_all)]
async fn resolve_departure(
	services: &Services,
	room_id: &RoomId,
	snapshot: Option<ShortStateHash>,
	departure: Option<PduEvent>,
) -> Departure {
	if let Some(snapshot) = snapshot {
		return Departure {
			once_joined: true,
			event: departure,
			after: Some(snapshot),
			left: Some(snapshot),
		};
	}

	let recorded = |event_id| {
		services
			.state
			.pdu_shortstatehash(event_id)
			.map(NotFound::present)
			.inspect_err(move |error| warn!(target: "tuwunel_api_client::sync::v3", %room_id, %event_id, %error, "Unreadable departure snapshot"))
			.map(|anchor| anchor.ok().flatten())
	};

	let anchor = departure
		.as_ref()
		.map(Event::event_id)
		.map_async(recorded)
		.map(Option::flatten)
		.await;

	Departure {
		once_joined: false,
		event: departure,
		after: anchor,
		left: anchor,
	}
}

/// Reports whether the timeline starts no later than the departure.
///
/// An unknown start or departure count reports false.
async fn starts_by_departure(
	services: &Services,
	room_id: &RoomId,
	first: Option<PduCount>,
	departure: Option<&PduEvent>,
) -> bool {
	let Some((first, event_id)) = first.zip(departure.map(Event::event_id)) else {
		return false;
	};

	services
		.timeline
		.get_pdu_count(event_id)
		.map(NotFound::present)
		.inspect_err(|error| warn!(target: "tuwunel_api_client::sync::v3", %room_id, %event_id, %error, "Unreadable departure count"))
		.map(|departed_at| {
			departed_at
				.ok()
				.flatten()
				.is_some_and(|departed_at| first <= departed_at)
		})
		.await
}

/// Decides whether the requester may see more of a left room's state than their
/// own membership.
///
/// Only a recorded snapshot is judged by its own visibility; current state
/// standing in for a missing one grants nothing.
async fn may_see_room_state(
	services: &Services,
	once_joined: bool,
	recorded: Option<ShortStateHash>,
) -> bool {
	once_joined
		|| recorded
			.map_async(|shortstatehash| {
				services
					.state_accessor
					.history_visibility(shortstatehash)
					.eq(&HistoryVisibility::WorldReadable)
			})
			.map(Option::unwrap_or_default)
			.await
}

/// Builds the state a left room's response draws from.
///
/// A requester confined to their own membership skips building the room's
/// state: the legacy field takes their membership in the snapshot served, and
/// `state_after` only the departure the caller applies. Current state stands in
/// for a missing recorded snapshot only where a snapshot is read.
#[tracing::instrument(level = "trace", skip_all)]
async fn served_state(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	ServedState {
		sees_room_state,
		recorded,
		full_state,
		state_after,
		since,
		horizon,
		after,
	}: ServedState,
) -> Result<impl Iterator<Item = PduEvent>> {
	let served = async || match recorded {
		| Some(recorded) => Ok(recorded),
		| None =>
			services
				.state
				.get_room_shortstatehash(room_id)
				.map_err(|_| missing_room_state(room_id))
				.await,
	};

	if sees_room_state {
		let params = StateChangeParams {
			full_state,
			state_after,
			since_shortstatehash: since,
			horizon_shortstatehash: horizon,
			after_shortstatehash: after,
			current_shortstatehash: served().await?,
			joined_since_last_sync: false,
			witness: None,
			include_heroes: true,
		};

		return calculate_state_changes(services, sender_user, room_id, params)
			.map_ok(|changes| Either::Left(changes.state_events.into_iter()))
			.await;
	}

	let baseline = since.filter(|_| !full_state);
	let membership = state_after
		.requested()
		.is_false()
		.then_async(async || {
			let served = served().await?;

			Ok::<_, Error>(own_membership(services, sender_user, served, baseline).await)
		})
		.map(Option::transpose)
		.map_ok(Option::flatten)
		.await?;

	Ok(Either::Right(membership.into_iter()))
}

/// Returns the requester's membership event when a state response includes it.
///
/// A full response always includes it, and an incremental one only when it
/// changed since the client's position.
#[tracing::instrument(level = "trace", skip_all)]
async fn own_membership(
	services: &Services,
	sender_user: &UserId,
	shortstatehash: ShortStateHash,
	since: Option<ShortStateHash>,
) -> Option<PduEvent> {
	if since == Some(shortstatehash) {
		return None;
	}

	let member = |shortstatehash| {
		services
			.state_accessor
			.state_get_shortid(shortstatehash, &StateEventType::RoomMember, sender_user.as_str())
			.map(NotFound::present)
			.inspect_err(move |error| warn!(target: "tuwunel_api_client::sync::v3", %sender_user, %shortstatehash, %error, "Unreadable membership in a left room's snapshot"))
	};

	let current = member(shortstatehash)
		.map(|current| current.ok().flatten())
		.await?;

	// A failed baseline read cannot show a change, so nothing is sent.
	let previous = since
		.try_map_async(member)
		.map_ok(Option::flatten)
		.map(Result::ok)
		.await?;

	previous
		.ne(&Some(current))
		.then_async(|| {
			services
				.timeline
				.get_pdu_from_shorteventid(current)
				.ok()
		})
		.map(Option::flatten)
		.await
}

/// A user who never joined gets only the state describing their own membership
/// unless the served snapshot is world-readable.
///
/// The same rule covers the membership a zero timeline limit moves into state.
fn visible_state(sender_user: &UserId, sees_room_state: bool) -> impl Fn(&PduEvent) -> bool {
	move |pdu| sees_room_state || pdu.is_type_and_state_key(&RoomMember, sender_user.as_str())
}

fn timeline_events<'a, Pdus>(
	services: &'a Services,
	timeline_pdus: Pdus,
	trailing: Option<PduEvent>,
	TimelineParams { sender_user, filter, limit, encrypted }: TimelineParams<'a>,
) -> impl Stream<Item = PduEvent> + Send + 'a
where
	Pdus: IntoIterator<Item = (PduCount, PduEvent)>,
	Pdus::IntoIter: Send + 'a,
{
	let trailing = trailing
		.into_iter()
		.stream()
		.wide_then(move |pdu| with_membership(services, pdu, sender_user, encrypted));

	timeline_pdus
		.into_iter()
		.stream()
		.wide_filter_map(move |item| ignored_filter(services, item, sender_user))
		.map(at!(1))
		.ready_filter(move |pdu| filter.room.timeline.matches(pdu))
		.take(limit)
		.wide_then(move |pdu| with_membership(services, pdu, sender_user, encrypted))
		.wide_then(move |pdu| {
			services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
		})
		.chain(trailing)
}

/// Records a failed horizon read, quietly when the row is simply absent.
///
/// `append_to_state` writes no state hash for a room's `m.room.create` event,
/// there being no state before it, so the first window of every room misses
/// here by design. An absent row reads the same whatever left it absent, so
/// the loud path is what remains: a fault that is not a miss at all.
pub(super) fn log_horizon_error(error: &Error) {
	match error.is_not_found() {
		| true =>
			debug!(target: "tuwunel_api_client::sync::v3", ?error, "no shortstatehash at the window's first event"),
		| false => inspect_debug_log(error),
	}
}

pub(super) fn in_timeline(
	timeline_pdus: &[(PduCount, PduEvent)],
) -> impl Fn(&PduEvent) -> bool + use<> {
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

#[cfg(test)]
mod tests {
	use ruma::user_id;
	use serde_json::{from_value, json};

	use super::*;

	#[test]
	fn left_state_without_room_access_keeps_only_own_membership() {
		let user = user_id!("@member:example.org");
		let event = |kind, key| {
			from_value(json!({
				"event_id": "$state:example.org", "room_id": "!r:example.org",
				"sender": user, "type": kind, "state_key": key, "content": {},
				"origin_server_ts": 1, "depth": 1, "prev_events": [], "auth_events": [],
				"hashes": {"sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}
			}))
			.expect("state fixture")
		};

		let events = [
			event("m.room.member", user.as_str()),
			event("m.room.member", "@other:example.org"),
			event("m.room.topic", ""),
		];

		let own_only = visible_state(user, false);

		assert!(own_only(&events[0]));
		assert!(!own_only(&events[1]));
		assert!(!own_only(&events[2]));
		assert!(events.iter().all(visible_state(user, true)));
	}
}
