use axum::extract::State;
use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt,
	future::{OptionFuture, join, join3, try_join3},
};
use ruma::{
	DeviceId, EventId, OwnedEventId, RoomId, UInt, UserId,
	api::client::{context::get_context, filter::RoomEventFilter},
	events::{AnyStateEvent, StateEventType},
	serde::Raw,
};
use tuwunel_core::{
	Err, Event, Result, at, debug_warn, err,
	matrix::pdu::{PduEvent, RawPduId},
	ref_at,
	utils::{
		BoolExt, IterStream,
		future::TryExtExt,
		stream::{BroadbandExt, ReadyExt, TryIgnore, WidebandExt},
	},
};
use tuwunel_service::{
	Services,
	rooms::{
		lazy_loading,
		lazy_loading::{Options, Witness},
		short::{ShortRoomId, ShortStateKey},
		timeline::PdusIterItem,
	},
};

use crate::{
	Ruma,
	client::{
		is_ignored_pdu,
		message::{
			add_membership_unsigned, event_filter, event_filters, ignored_filter,
			lazy_loading_witness, related_by_filter, with_membership,
		},
	},
};

const LIMIT_MAX: usize = 100;
const LIMIT_DEFAULT: usize = 10;

/// # `GET /_matrix/client/r0/rooms/{roomId}/context/{eventId}`
///
/// Allows loading room history around an event.
///
/// - Only works if the user is joined (TODO: always allow, but only show events
///   if the user was joined, depending on history_visibility)
pub(crate) async fn get_context_route(
	State(services): State<crate::State>,
	body: Ruma<get_context::v3::Request>,
) -> Result<get_context::v3::Response> {
	event_context(&services, ContextArgs {
		room_id: &body.room_id,
		event_id: &body.event_id,
		sender_user: body.sender_user(),
		sender_device: body.sender_device.as_deref(),
		filter: &body.filter,
		limit: Some(body.limit),
		bypass_visibility: false,
	})
	.await
}

/// Shared inputs for [`event_context`], the core behind both the client-server
/// `/context` route and the admin room-context endpoint.
pub(crate) struct ContextArgs<'a> {
	pub room_id: &'a RoomId,
	pub event_id: &'a EventId,
	pub sender_user: &'a UserId,
	pub sender_device: Option<&'a DeviceId>,
	pub filter: &'a RoomEventFilter,
	pub limit: Option<UInt>,

	/// Skip the base-event visibility and ignore checks and the surrounding
	/// halves' visibility and ignore filters, for admin callers.
	pub bypass_visibility: bool,
}

/// Loads the timeline window around an event with its state and aggregations,
/// applying (unless `bypass_visibility`) the per-user visibility and ignore
/// checks. Powers the client-server `/context` route and its admin bypass twin.
pub(crate) async fn event_context(
	services: &Services,
	args: ContextArgs<'_>,
) -> Result<get_context::v3::Response> {
	let ContextArgs {
		room_id,
		event_id,
		sender_user,
		sender_device,
		filter,
		limit,
		bypass_visibility,
	} = args;

	if !services.metadata.exists(room_id).await {
		return Err!(Request(Forbidden("Room does not exist to this server")));
	}

	let limit: usize = limit
		.and_then(|limit| limit.try_into().ok())
		.unwrap_or(LIMIT_DEFAULT)
		.min(LIMIT_MAX);

	let (base_id, base_pdu) =
		resolve_base_event(services, room_id, event_id, sender_user, bypass_visibility).await?;

	let base_count = base_id.pdu_count();

	let encrypted = services
		.state_accessor
		.is_encrypted_room(room_id)
		.await;

	let shortroomid = services.short.get_shortroomid(room_id).await?;

	let base_event = async {
		let item = if bypass_visibility {
			(base_count, base_pdu)
		} else {
			ignored_filter(services, (base_count, base_pdu), sender_user).await?
		};

		Some(add_membership_unsigned(services, item, sender_user, encrypted).await)
	};

	let half = TimelineHalf {
		services,
		filter,
		shortroomid,
		sender_user,
		encrypted,
		bypass_visibility,
	};

	let events_before = collect_timeline_half(
		half,
		services
			.timeline
			.pdus_rev(Some(sender_user), room_id, Some(base_count)),
		limit / 2,
	);

	let events_after = collect_timeline_half(
		half,
		services
			.timeline
			.pdus(Some(sender_user), room_id, Some(base_count)),
		limit.div_ceil(2),
	);

	let (base_event, events_before, events_after): (_, Vec<_>, Vec<_>) =
		join3(base_event, events_before, events_after)
			.boxed()
			.await;

	let lazy_loading_context = lazy_loading::Context {
		user_id: sender_user,
		device_id: sender_device,
		room_id,
		token: Some(base_count.into_unsigned()),
		options: Some(&filter.lazy_load_options),
		mode: lazy_loading::Mode::Update,
	};

	let lazy_loading_witnessed = filter
		.lazy_load_options
		.is_enabled()
		.then_async(|| {
			let witnessed = base_event
				.iter()
				.chain(events_before.iter())
				.chain(events_after.iter());

			lazy_loading_witness(services, &lazy_loading_context, witnessed)
		});

	let state_at = events_after
		.last()
		.map(ref_at!(1))
		.map_or_else(|| event_id, |pdu| pdu.event_id.as_ref());

	let (lazy_loading_witnessed, state_ids) =
		join(lazy_loading_witnessed, load_state_ids(services, room_id, state_at)).await;

	let state = build_state_response(
		services,
		state_ids?,
		lazy_loading_witnessed.unwrap_or_default(),
		filter,
		sender_user,
		encrypted,
	)
	.await;

	let event = OptionFuture::from(base_event.map(at!(1)).map(|pdu| {
		services
			.pdu_metadata
			.bundle_aggregations(sender_user, pdu)
	}))
	.await
	.map(Event::into_format);

	Ok(get_context::v3::Response {
		event,

		start: events_before
			.last()
			.map(at!(0))
			.or(Some(base_count))
			.as_ref()
			.map(ToString::to_string),

		// `end` is one past the base so a backward page from it still yields the base;
		// `start` stays at `base_count` (a bare count can't suit both directions).
		end: events_after
			.last()
			.map(at!(0))
			.or_else(|| Some(base_count.saturating_add(1)))
			.as_ref()
			.map(ToString::to_string),

		events_before: events_before
			.into_iter()
			.map(at!(1))
			.map(Event::into_format)
			.collect(),

		events_after: events_after
			.into_iter()
			.map(at!(1))
			.map(Event::into_format)
			.collect(),

		state,
	})
}

async fn resolve_base_event(
	services: &Services,
	room_id: &RoomId,
	event_id: &EventId,
	sender_user: &UserId,
	bypass_visibility: bool,
) -> Result<(RawPduId, PduEvent)> {
	let lookup = || {
		let base_id = services
			.timeline
			.get_pdu_id(event_id)
			.map_err(|_| err!(Request(NotFound("Event not found."))));

		let base_pdu = services
			.timeline
			.get_pdu(event_id)
			.map_err(|_| err!(Request(NotFound("Base event not found."))));

		let visible = services
			.state_accessor
			.user_can_see_event(sender_user, room_id, event_id)
			.map(Ok);

		try_join3(base_id, base_pdu, visible)
	};

	let resolve_remote = services
		.config
		.fetch_unreceived_contexts_over_federation
		&& services.config.allow_federation;

	let (base_id, base_pdu, visible) = match lookup().await {
		| Ok(found) => found,
		| Err(e) if !resolve_remote => return Err(e),
		| Err(_) => {
			services
				.timeline
				.fetch_remote_event(room_id, event_id)
				.await
				.ok();

			lookup().await?
		},
	};

	if base_pdu.room_id != *room_id || base_pdu.event_id != *event_id {
		return Err!(Request(NotFound("Base event not found.")));
	}

	if !bypass_visibility && !visible {
		debug_warn!(
			req_evt = ?event_id, ?base_id, ?room_id,
			"Event requested by {sender_user} but is not allowed to see it."
		);

		return Err!(Request(NotFound("Event not found.")));
	}

	if !bypass_visibility && is_ignored_pdu(services, &base_pdu, sender_user).await {
		return Err!(HttpJson(NOT_FOUND, {
			"errcode": "M_SENDER_IGNORED",
			"error": "You have ignored the user that sent this event",
			"sender": base_pdu.sender().as_str(),
		}));
	}

	Ok((base_id, base_pdu))
}

/// Shared inputs for the two [`collect_timeline_half`] calls assembling the
/// before and after windows; only the stream and take count differ per call.
#[derive(Clone, Copy)]
struct TimelineHalf<'a> {
	services: &'a Services,
	filter: &'a RoomEventFilter,
	shortroomid: ShortRoomId,
	sender_user: &'a UserId,
	encrypted: bool,
	bypass_visibility: bool,
}

async fn collect_timeline_half<'a, S>(
	half: TimelineHalf<'a>,
	pdus: S,
	take: usize,
) -> Vec<PdusIterItem>
where
	S: Stream<Item = Result<PdusIterItem>> + Send + 'a,
{
	let TimelineHalf {
		services,
		filter,
		shortroomid,
		sender_user,
		encrypted,
		bypass_visibility,
	} = half;

	pdus.ignore_err()
		.ready_filter_map(|item| event_filter(item, filter))
		.wide_filter_map(|item| related_by_filter(services, shortroomid, filter, item))
		.wide_filter_map(|item| event_filters(services, sender_user, item, bypass_visibility))
		.take(take)
		.wide_then(|item| add_membership_unsigned(services, item, sender_user, encrypted))
		.wide_then(async |(count, pdu)| {
			let pdu = services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
				.await;

			(count, pdu)
		})
		.collect()
		.await
}

async fn load_state_ids(
	services: &Services,
	room_id: &RoomId,
	state_at: &EventId,
) -> Result<Vec<(ShortStateKey, OwnedEventId)>> {
	services
		.state
		.pdu_shortstatehash(state_at)
		.or_else(|_| services.state.get_room_shortstatehash(room_id))
		.map_ok(|shortstatehash| {
			services
				.state_accessor
				.state_full_ids(shortstatehash)
				.map(Ok)
		})
		.map_err(|e| err!(Database("State not found: {e}")))
		.try_flatten_stream()
		.try_collect()
		.boxed()
		.await
}

async fn build_state_response(
	services: &Services,
	state_ids: Vec<(ShortStateKey, OwnedEventId)>,
	lazy_loading_witnessed: Witness,
	filter: &RoomEventFilter,
	sender_user: &UserId,
	encrypted: bool,
) -> Vec<Raw<AnyStateEvent>> {
	let shortstatekeys = state_ids.iter().map(at!(0)).stream();
	let shorteventids = state_ids.iter().map(ref_at!(1)).stream();

	services
		.short
		.multi_get_statekey_from_short(shortstatekeys)
		.zip(shorteventids)
		.ready_filter_map(|item| Some((item.0.ok()?, item.1)))
		.ready_filter_map(|((event_type, state_key), event_id)| {
			if filter.lazy_load_options.is_enabled()
				&& event_type == StateEventType::RoomMember
				&& state_key
					.as_str()
					.try_into()
					.is_ok_and(|user_id: &UserId| !lazy_loading_witnessed.contains(user_id))
			{
				return None;
			}

			Some(event_id)
		})
		.broad_filter_map(|event_id: &OwnedEventId| {
			services.timeline.get_pdu(event_id.as_ref()).ok()
		})
		.broad_then(|pdu| with_membership(services, pdu, sender_user, encrypted))
		.map(Event::into_format)
		.collect()
		.await
}
