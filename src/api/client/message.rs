use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, future::Either, pin_mut};
use ruma::{
	DeviceId, RoomId, UInt, UserId,
	api::{
		Direction,
		client::{filter::RoomEventFilter, message::get_message_events},
	},
	events::{
		AnyStateEvent, StateEventType, TimelineEventType, TimelineEventType::*,
		relation::RelationType,
	},
	serde::Raw,
};
use tuwunel_core::{
	Err, PduId, Result, at,
	matrix::{
		event::{Event, Matches},
		pdu::{PduCount, PduEvent},
	},
	ref_at,
	smallvec::SmallVec,
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
		short::ShortRoomId,
		timeline::PdusIterItem,
	},
};

use crate::Ruma;

/// Shared inputs for [`get_messages`], the pagination core behind both the
/// client-server `/messages` route and the admin room-messages endpoint.
pub(crate) struct MessagesArgs<'a> {
	pub room_id: &'a RoomId,
	pub sender_user: &'a UserId,
	pub sender_device: Option<&'a DeviceId>,
	pub from: Option<&'a str>,
	pub to: Option<&'a str>,
	pub dir: Direction,
	pub limit: Option<UInt>,
	pub filter: &'a RoomEventFilter,

	/// Skip the room-visibility gate and the per-event visibility and ignore
	/// filters, for admin callers that see all history.
	pub bypass_visibility: bool,
}

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

/// MSC3440 `related_by_rel_types` entries, typed at the compare boundary.
type RelTypes = SmallVec<[RelationType; 1]>;

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
	get_messages(&services, MessagesArgs {
		room_id: &body.room_id,
		sender_user: body.sender_user(),
		sender_device: body.sender_device.as_deref(),
		from: body.from.as_deref(),
		to: body.to.as_deref(),
		dir: body.dir,
		limit: Some(body.limit),
		filter: &body.filter,
		bypass_visibility: false,
	})
	.await
}

/// Paginates a room's timeline, applying the request filter and (unless
/// `bypass_visibility`) the per-user visibility and ignore filters. Powers the
/// client-server `/messages` route and its admin bypass twin.
pub(crate) async fn get_messages(
	services: &Services,
	args: MessagesArgs<'_>,
) -> Result<get_message_events::v3::Response> {
	let MessagesArgs {
		room_id,
		sender_user,
		sender_device,
		from,
		to,
		dir,
		limit,
		filter,
		bypass_visibility,
	} = args;

	if !services.metadata.exists(room_id).await {
		return Err!(Request(Forbidden("Room does not exist to this server")));
	}

	if !bypass_visibility
		&& !services
			.state_accessor
			.user_can_see_room(sender_user, room_id)
			.await
	{
		return Err!(Request(Forbidden("You don't have permission to view this room.")));
	}

	let from: PduCount = from
		.map(str::parse)
		.transpose()?
		.unwrap_or_else(|| match dir {
			| Direction::Forward => PduCount::min(),
			| Direction::Backward => PduCount::max(),
		});

	let to: Option<PduCount> = to.map(str::parse).flat_ok();

	let limit: usize = limit
		.and_then(|limit| limit.try_into().ok())
		.unwrap_or(LIMIT_DEFAULT)
		.min(LIMIT_MAX);

	if matches!(dir, Direction::Backward) {
		services
			.timeline
			.backfill_if_required(room_id, from)
			.await
			.log_err()
			.ok();
	}

	let it = match dir {
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

	let shortroomid = services.short.get_shortroomid(room_id).await?;

	let events: Vec<_> = it
		.ready_take_while(|(count, _)| Some(*count) != to)
		.ready_filter_map(|item| event_filter(item, filter))
		.wide_filter_map(|item| related_by_filter(services, shortroomid, filter, item))
		.wide_filter_map(|item| event_filters(services, sender_user, item, bypass_visibility))
		.take(limit)
		.wide_then(|item| add_membership_unsigned(services, item, sender_user, encrypted))
		.wide_then(async |(count, pdu)| {
			let pdu = services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
				.await;

			(count, pdu)
		})
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
		.then_async(|| lazy_loading_witness(services, &lazy_loading_context, events.iter()));

	let state = witness
		.map(Option::into_iter)
		.map(|option| option.flat_map(Witness::into_iter))
		.map(IterStream::stream)
		.into_stream()
		.flatten()
		.broad_filter_map(async |user_id| get_member_event(services, room_id, &user_id).await)
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

pub(crate) async fn event_filters(
	services: &Services,
	user_id: &UserId,
	item: PdusIterItem,
	bypass_visibility: bool,
) -> Option<PdusIterItem> {
	if bypass_visibility {
		return Some(item);
	}

	let item = ignored_filter(services, item, user_id).await?;
	let item = visibility_filter(services, item, user_id).await?;

	Some(item)
}

/// MSC3440 `related_by_*`: include an event only when another event relates
/// to it matching the filter's reverse-relation criteria. A no-op stage when
/// the filter carries neither field.
pub(crate) async fn related_by_filter(
	services: &Services,
	shortroomid: ShortRoomId,
	filter: &RoomEventFilter,
	item: PdusIterItem,
) -> Option<PdusIterItem> {
	if filter.related_by_senders.is_empty() && filter.related_by_rel_types.is_empty() {
		return Some(item);
	}

	let rel_types: RelTypes = filter
		.related_by_rel_types
		.iter()
		.map(String::as_str)
		.map(RelationType::from)
		.collect();

	let (count, _) = &item;
	let target = PduId { shortroomid, count: *count };

	services
		.pdu_metadata
		.has_incoming_relation(target, &filter.related_by_senders, &rel_types)
		.await
		.then_some(item)
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

	let ignored_server = services
		.config
		.is_forbidden_remote_server_name(event.sender().server_name());

	ignored_server
		|| services
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

#[cfg_attr(debug_assertions, tuwunel_core::ctor(unsafe))]
fn _is_sorted() {
	debug_assert!(
		IGNORED_MESSAGE_TYPES.is_sorted(),
		"IGNORED_MESSAGE_TYPES must be sorted by the developer"
	);
}
