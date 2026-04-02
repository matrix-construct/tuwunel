use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, future::Either, pin_mut};
use ruma::{
	RoomId, UserId,
	api::{
		Direction,
		client::{filter::RoomEventFilter, message::get_message_events},
	},
	events::{AnyStateEvent, StateEventType, TimelineEventType, TimelineEventType::*},
	serde::Raw,
};
use tuwunel_core::{
	Err, Result, at,
	matrix::{
		event::{Event, Matches},
		pdu::{PduCount, PduEvent},
	},
	ref_at,
	utils::{
		BoolExt, IterStream, ReadyExt,
		result::{FlatOk, LogErr},
		stream::{BroadbandExt, TryIgnore, WidebandExt},
	},
};
use tuwunel_service::{
	Services,
	rooms::{
		lazy_loading,
		lazy_loading::{Options, Witness},
		timeline::PdusIterItem,
	},
};

use crate::Ruma;

/// list of safe and common non-state events to ignore if the user is ignored.
/// MUST be sorted by `TimelineEventType::event_type_str()` for `binary_search`.
const IGNORED_MESSAGE_TYPES: &[TimelineEventType] = &[
	CallInvite,           // m.call.invite
	KeyVerificationStart, // m.key.verification.start
	Location,             // m.location
	PollStart,            // m.poll.start
	Reaction,             // m.reaction
	RoomEncrypted,        // m.room.encrypted
	RoomMessage,          // m.room.message
	Sticker,              // m.sticker
	Audio,                // org.matrix.msc1767.audio
	Emote,                // org.matrix.msc1767.emote
	File,                 // org.matrix.msc1767.file
	Image,                // org.matrix.msc1767.image
	Video,                // org.matrix.msc1767.video
	Voice,                // org.matrix.msc3245.voice.v2
	UnstablePollStart,    // org.matrix.msc3381.poll.start
	Beacon,               // org.matrix.msc3672.beacon
	CallNotify,           // org.matrix.msc4075.call.notify
];

const LIMIT_MAX: usize = 1000;
const LIMIT_DEFAULT: usize = 10;

/// # `GET /_matrix/client/r0/rooms/{roomId}/messages`
///
/// Allows paginating through room history.
///
/// - Only works if the user is joined (TODO: always allow, but only show events
///   where the user was joined, depending on `history_visibility`)
pub(crate) async fn get_message_events_route(
	State(services): State<crate::State>,
	body: Ruma<get_message_events::v3::Request>,
) -> Result<get_message_events::v3::Response> {
	let sender_user = body.sender_user();
	let sender_device = body.sender_device.as_deref();
	let room_id = &body.room_id;
	let filter = &body.filter;

	if !services.metadata.exists(room_id).await {
		return Err!(Request(Forbidden("Room does not exist to this server")));
	}

	let from: PduCount = body
		.from
		.as_deref()
		.map(str::parse)
		.transpose()?
		.unwrap_or_else(|| match body.dir {
			| Direction::Forward => PduCount::min(),
			| Direction::Backward => PduCount::max(),
		});

	let to: Option<PduCount> = body.to.as_deref().map(str::parse).flat_ok();

	let limit: usize = body
		.limit
		.try_into()
		.unwrap_or(LIMIT_DEFAULT)
		.min(LIMIT_MAX);

	if matches!(body.dir, Direction::Backward) {
		services
			.timeline
			.backfill_if_required(room_id, from)
			.await
			.log_err()
			.ok();
	}

	let it = match body.dir {
		| Direction::Forward => Either::Left(
			services
				.timeline
				.pdus(Some(sender_user), room_id, Some(from))
				.ignore_err(),
		),
		| Direction::Backward => Either::Right(
			services
				.timeline
				.pdus_rev(Some(sender_user), room_id, Some(from))
				.ignore_err(),
		),
	};

	let encrypted = services
		.state_accessor
		.is_encrypted_room(room_id)
		.await;

	let events: Vec<_> = it
		.ready_take_while(|(count, _)| Some(*count) != to)
		.ready_filter_map(|item| event_filter(item, filter))
		.wide_filter_map(|item| event_filters(&services, sender_user, item))
		.take(limit)
		.wide_then(|item| add_membership_unsigned(&services, item, sender_user, encrypted))
		.collect()
		.await;

	let lazy_loading_context = lazy_loading::Context {
		user_id: sender_user,
		device_id: sender_device,
		room_id,
		token: Some(from.into_unsigned()),
		options: Some(&filter.lazy_load_options),
		mode: lazy_loading::Mode::Update,
	};

	let witness = filter
		.lazy_load_options
		.is_enabled()
		.then_async(|| lazy_loading_witness(&services, &lazy_loading_context, events.iter()));

	let state = witness
		.map(Option::into_iter)
		.map(|option| option.flat_map(Witness::into_iter))
		.map(IterStream::stream)
		.into_stream()
		.flatten()
		.broad_filter_map(async |user_id| get_member_event(&services, room_id, &user_id).await)
		.collect()
		.await;

	let next_token = events.last().map(at!(0));

	let chunk = events
		.into_iter()
		.map(at!(1))
		.map(Event::into_format)
		.collect();

	Ok(get_message_events::v3::Response {
		start: from.to_string(),
		end: next_token.as_ref().map(ToString::to_string),
		chunk,
		state,
	})
}

pub(crate) async fn lazy_loading_witness<'a, I>(
	services: &Services,
	lazy_loading_context: &lazy_loading::Context<'_>,
	events: I,
) -> Witness
where
	I: Iterator<Item = &'a PdusIterItem> + Clone + Send,
{
	let oldest = events
		.clone()
		.map(|(count, _)| count)
		.copied()
		.min()
		.unwrap_or_else(PduCount::max);

	let newest = events
		.clone()
		.map(|(count, _)| count)
		.copied()
		.max()
		.unwrap_or_else(PduCount::max);

	let receipts = services.read_receipt.readreceipts_since(
		lazy_loading_context.room_id,
		oldest.into_unsigned(),
		Some(newest.into_unsigned()),
	);

	pin_mut!(receipts);
	let witness: Witness = events
		.stream()
		.map(ref_at!(1))
		.map(Event::sender)
		.map(ToOwned::to_owned)
		.chain(
			receipts
				.ready_take_while(|(_, c, _)| *c <= newest.into_unsigned())
				.map(|(user_id, ..)| user_id.to_owned()),
		)
		.collect()
		.await;

	services
		.lazy_loading
		.witness_retain(witness, lazy_loading_context)
		.await
}

async fn get_member_event(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
) -> Option<Raw<AnyStateEvent>> {
	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
		.map_ok(Event::into_format)
		.await
		.ok()
}

async fn event_filters(
	services: &Services,
	user_id: &UserId,
	item: PdusIterItem,
) -> Option<PdusIterItem> {
	let item = ignored_filter(services, item, user_id).await?;
	let item = visibility_filter(services, item, user_id).await?;

	Some(item)
}

#[inline]
pub(crate) async fn ignored_filter(
	services: &Services,
	item: PdusIterItem,
	user_id: &UserId,
) -> Option<PdusIterItem> {
	let (_, ref pdu) = item;

	is_ignored_pdu(services, pdu, user_id)
		.await
		.is_false()
		.then_some(item)
}

#[inline]
pub(crate) async fn is_ignored_pdu<Pdu>(
	services: &Services,
	event: &Pdu,
	user_id: &UserId,
) -> bool
where
	Pdu: Event,
{
	// exclude Synapse's dummy events from bloating up response bodies. clients
	// don't need to see this.
	if event.kind().to_cow_str() == "org.matrix.dummy_event" {
		return true;
	}

	if IGNORED_MESSAGE_TYPES
		.binary_search(event.kind())
		.is_err()
	{
		return false;
	}

	services
		.users
		.user_is_ignored(event.sender(), user_id)
		.await
}

#[inline]
pub(crate) async fn visibility_filter(
	services: &Services,
	item: PdusIterItem,
	user_id: &UserId,
) -> Option<PdusIterItem> {
	let (_, pdu) = &item;

	services
		.state_accessor
		.user_can_see_event(user_id, pdu.room_id(), pdu.event_id())
		.await
		.then_some(item)
}

#[inline]
pub(crate) fn event_filter(item: PdusIterItem, filter: &RoomEventFilter) -> Option<PdusIterItem> {
	let (_, pdu) = &item;
	filter.matches(pdu).then_some(item)
}

/// MSC4115: stamp `unsigned.membership` on a served PDU with the requesting
/// user's membership at the time of the event. The MSC permits omitting the
/// property when calculating it is expensive, so the project restricts it to
/// encrypted rooms where membership-vs-event ordering matters for key share.
#[inline]
pub(crate) async fn annotate_membership(
	services: &Services,
	pdu: &mut PduEvent,
	user_id: &UserId,
	encrypted: bool,
) {
	if !encrypted {
		return;
	}

	let membership = services
		.state_accessor
		.user_membership_at_pdu(user_id, pdu)
		.await;

	pdu.add_membership(&membership).log_err().ok();
}

/// `annotate_membership` consume-and-return adapter for stream chains.
#[inline]
pub(crate) async fn with_membership(
	services: &Services,
	mut pdu: PduEvent,
	user_id: &UserId,
	encrypted: bool,
) -> PduEvent {
	annotate_membership(services, &mut pdu, user_id, encrypted).await;
	pdu
}

/// `with_membership` adapter for timeline-iterator items.
#[inline]
pub(crate) async fn add_membership_unsigned(
	services: &Services,
	(count, pdu): PdusIterItem,
	user_id: &UserId,
	encrypted: bool,
) -> PdusIterItem {
	(count, with_membership(services, pdu, user_id, encrypted).await)
}

#[cfg_attr(debug_assertions, tuwunel_core::ctor)]
fn _is_sorted() {
	debug_assert!(
		IGNORED_MESSAGE_TYPES.is_sorted(),
		"IGNORED_MESSAGE_TYPES must be sorted by the developer"
	);
}
