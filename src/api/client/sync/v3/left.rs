use std::collections::BTreeMap;

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join3, join4},
};
use ruma::{
	OwnedEventId, OwnedRoomId, RoomId, UserId,
	api::client::{
		filter::FilterDefinition,
		sync::sync_events::v3::{LeftRoom, RoomAccountData, StateEvents, Timeline},
	},
	events::{AnyRawAccountDataEvent, TimelineEventType::*, room::member::MembershipState},
};
use tuwunel_core::{
	Error, Result, at, debug,
	error::inspect_debug_log,
	extract_variant, is_equal_to, ref_at,
	result::FlatOk,
	smallvec::SmallVec,
	utils::{
		BoolExt, FutureBoolExt, IterStream, ReadyExt, TryFutureExtExt,
		future::ReadyBoolExt,
		option::OptionExt,
		result::MapExpect,
		stream::{BroadbandExt, WidebandExt},
	},
	warn,
};
use tuwunel_matrix::{
	Event,
	event::{Matches, trim_event_fields},
	pdu::{PduCount, PduEvent},
};
use tuwunel_service::Services;

use super::{
	super::{
		load_timeline, own_departure, stand_in_leave, strip_prev_state, timeline_prev_batch,
	},
	StateAfter, StateChangeParams, StateChanges, missing_room_state,
	state::calculate_state_changes,
};
use crate::{ignored_filter, is_empty_account_data_event, with_membership};

type TimelineEventIds = SmallVec<[OwnedEventId; 1]>;

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

	// Last, so a refusal on the same poll spares its reads.
	let is_out_of_band = departed_out_of_band(services, room_id, sender_user, since);

	if forgotten
		|| is_not_found
			.or3(is_disabled, is_banned, is_out_of_band)
			.await
	{
		let departure = forgotten
			.then_async(async || {
				own_departure(services, room_id, sender_user)
					.await
					.map(Event::into_format)
			})
			.await
			.flatten();

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

	let horizon = services
		.state_accessor
		.departure_count(sender_user, room_id)
		.await
		.inspect_err(|error| warn!(target: "tuwunel_api_client::sync::v3", %room_id, %sender_user, %error, "Unreadable departure count"))
		.ok()
		.flatten()
		.map(PduCount::into_unsigned)
		.unwrap_or(left_count);

	load_left_room(
		services,
		sender_user,
		room_id,
		since,
		horizon,
		full_state,
		state_after,
		filter,
	)
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
	sender_user: &UserId,
	since: u64,
) -> bool {
	let no_later_event = since != 0
		&& services
			.timeline
			.last_timeline_count(None, room_id, None)
			.await
			.inspect_err(|error| warn!(target: "tuwunel_api_client::sync::v3", %room_id, %since, %error, "Unreadable last event count"))
			.is_ok_and(|last| last <= PduCount::Normal(since));

	own_departure(services, room_id, sender_user)
		.await
		.is_none_or(|event| {
			no_later_event && event.membership_for(sender_user) != Some(MembershipState::Ban)
		})
}

#[tracing::instrument(
	target = "tuwunel_api_client::sync::v3",
	name = "load",
	level = "debug",
	skip_all
)]
#[expect(clippy::too_many_arguments)]
async fn load_left_room(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	since: u64,
	horizon: u64,
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
		Some(PduCount::Normal(horizon)),
		timeline_limit.max(1),
	)
	.await
	.unwrap_or_default();

	let since_shortstatehash = since
		.lt(&horizon)
		.then_async(|| {
			services
				.timeline
				.next_shortstatehash(room_id, PduCount::Normal(since))
				.ok()
		})
		.map(Option::flatten);

	let horizon_shortstatehash = timeline_pdus
		.first()
		.map(at!(0))
		.map_async(|count| {
			services
				.timeline
				.get_shortstatehash(room_id, count)
				.inspect_err(log_horizon_error)
				.ok()
		});

	// MSC4222 `state_after` describes the state at the leave.
	let after_shortstatehash = state_after.requested().then_async(|| {
		services
			.timeline
			.shortstatehash_after(room_id, PduCount::Normal(horizon))
			.inspect_err(inspect_debug_log)
	});

	let left_shortstatehash = services
		.timeline
		.get_shortstatehash(room_id, PduCount::Normal(horizon))
		.inspect_err(inspect_debug_log)
		.or_else(|_| services.state.get_room_shortstatehash(room_id))
		.map_err(|_| missing_room_state(room_id));

	let (since_shortstatehash, horizon_shortstatehash, after_shortstatehash, left_shortstatehash) =
		join4(
			since_shortstatehash,
			horizon_shortstatehash,
			after_shortstatehash,
			left_shortstatehash,
		)
		.boxed()
		.await;

	let StateChanges { state_events, .. } =
		calculate_state_changes(services, sender_user, room_id, StateChangeParams {
			full_state: full_state || initial,
			state_after,
			since_shortstatehash,
			horizon_shortstatehash: horizon_shortstatehash.flatten(),
			after_shortstatehash: after_shortstatehash.flat_ok(),
			current_shortstatehash: left_shortstatehash?,
			joined_since_last_sync: false,
			witness: None,
			include_heroes: true,
		})
		.boxed()
		.await?;

	let is_sender_membership = |event: &PduEvent| {
		*event.kind() == RoomMember && event.state_key() == Some(sender_user.as_str())
	};

	let departure = late_departure(services, sender_user, room_id, since, horizon).await;

	let has_departure = departure.is_some();

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
		.filter(|pdu| !has_departure || !is_sender_membership(pdu))
		.chain(departure)
		.filter(|pdu| filter.room.state.matches(pdu))
		.filter(|pdu| timeline_limit > 0 || has_departure || !is_sender_membership(pdu))
		.chain(timeline_sender_member)
		.stream()
		.wide_then(|pdu| with_membership(services, pdu, sender_user, encrypted))
		.map(|pdu| strip_prev_state(pdu, sender_user, &in_timeline))
		.map(|pdu| trim_event_fields(pdu.into_format(), event_fields))
		.collect();

	let left_prev_batch = timeline_limit
		.eq(&0)
		.then_some(horizon)
		.map(PduCount::Normal);

	let prev_batch = timeline_prev_batch(&timeline_pdus, PduCount::Normal(horizon))
		.filter(|_| timeline_limit > 0)
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

async fn late_departure(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	since: u64,
	horizon: u64,
) -> Option<PduEvent> {
	since
		.ge(&horizon)
		.then_async(|| {
			services
				.timeline
				.get_pdu_at(room_id, PduCount::Normal(horizon))
				.inspect_err(inspect_debug_log)
				.ok()
		})
		.map(|event| {
			event.flatten().filter(|event| {
				matches!(
					event.membership_for(sender_user),
					Some(MembershipState::Leave | MembershipState::Ban)
				)
			})
		})
		.await
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
