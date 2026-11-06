use std::collections::BTreeMap;

use futures::{
	FutureExt, StreamExt, TryFutureExt, TryStreamExt,
	future::{join, join3, join4, try_join},
};
use ruma::{
	DeviceId, OwnedEventId, OwnedUserId, RoomId, UserId,
	api::client::filter::FilterDefinition,
	events::{
		AnySyncEphemeralRoomEvent, StateEventType, SyncEphemeralRoomEvent,
		TimelineEventType::*,
		room::member::{MembershipState, RoomMemberEventContent},
		typing::TypingEventContent,
	},
	serde::Raw,
};
use tuwunel_core::{
	Result, at,
	error::inspect_debug_log,
	is_false, ref_at,
	result::FlatOk,
	utils::{
		BoolExt, FutureBoolExt, IterStream, ReadyExt, TryFutureExtExt,
		future::{OptionStream, ReadyBoolExt},
		option::OptionExt,
		stream::{BroadbandExt, TryBroadbandExt, TryReadyExt},
	},
};
use tuwunel_matrix::{
	Event,
	pdu::{PduCount, PduEvent},
};
use tuwunel_service::{
	Services,
	rooms::{
		lazy_loading,
		lazy_loading::{Options, Witness},
		short::{ShortEventId, ShortStateKey},
	},
};

use super::{
	super::joined_since, StateAfter, StateChangeParams, StateChanges, joined::RoomMetadata,
	left::log_horizon_error, missing_room_state,
};

pub(super) struct UserMetadata {
	pub(super) witness: Option<Witness>,
	pub(super) last_notification_read: Option<u64>,
	pub(super) thread_last_reads: BTreeMap<OwnedEventId, u64>,
	pub(super) last_privateread_update: u64,
	pub(super) joined_since_last_sync: bool,
}

#[expect(clippy::too_many_arguments)]
pub(super) async fn gather_room_metadata(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	since: u64,
	next_batch: u64,
	timeline_pdus: &[(PduCount, PduEvent)],
	last_timeline_count: PduCount,
	include_state: bool,
	state_after: StateAfter,
) -> Result<RoomMetadata> {
	let since_shortstatehash = include_state.then_async(|| {
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
				.inspect_err(log_horizon_error)
		});

	// MSC4222 `state_after` describes the state at the end of the timeline.
	let after_shortstatehash = state_after.requested().then_async(|| {
		services
			.timeline
			.shortstatehash_after(room_id, last_timeline_count)
			.inspect_err(inspect_debug_log)
	});

	let current_shortstatehash = include_state.then_async(|| {
		// An empty timeline needs state after the last event, not before it.
		timeline_pdus
			.is_empty()
			.then(|| {
				services
					.timeline
					.next_shortstatehash(room_id, last_timeline_count)
					.left_future()
			})
			.unwrap_or_else(|| {
				services
					.timeline
					.get_shortstatehash(room_id, last_timeline_count)
					.inspect_err(inspect_debug_log)
					.right_future()
			})
			.or_else(|_| services.state.get_room_shortstatehash(room_id))
			.map_err(|_| missing_room_state(room_id))
	});

	let encrypted_room =
		include_state.then_async(|| services.state_accessor.is_encrypted_room(room_id));

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

#[expect(clippy::too_many_arguments)]
pub(super) async fn gather_user_metadata(
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

	let joined_since_last_sync =
		timeline_changed.then_async(|| joined_since(services, room_id, sender_user, since));

	// Busy rounds need these cursors too, to authorize an explicit zero count.
	let last_notification_read = services
		.pusher
		.last_notification_read(sender_user, room_id)
		.ok();

	let thread_last_reads = services
		.pusher
		.thread_last_notification_reads(sender_user, room_id);

	let last_privateread_update = services
		.read_receipt
		.last_privateread_update(sender_user, room_id);

	let (
		(last_privateread_update, last_notification_read, thread_last_reads),
		joined_since_last_sync,
		witness,
	) = join3(
		join3(last_privateread_update, last_notification_read, thread_last_reads),
		joined_since_last_sync,
		witness,
	)
	.await;

	UserMetadata {
		witness,
		last_notification_read,
		thread_last_reads,
		last_privateread_update,
		joined_since_last_sync: joined_since_last_sync.unwrap_or_default(),
	}
}

#[tracing::instrument(
	target = "tuwunel_api_client::sync::v3",
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
pub(super) async fn calculate_state_changes<'a>(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	StateChangeParams {
		full_state,
		state_after,
		since_shortstatehash,
		horizon_shortstatehash,
		after_shortstatehash,
		current_shortstatehash,
		joined_since_last_sync,
		witness,
		include_heroes,
	}: StateChangeParams<'a>,
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

	let get_pdu = |shorteventid: ShortEventId| {
		services
			.timeline
			.get_pdu_from_shorteventid(shorteventid)
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
			.boxed()
			.into_future()
	});

	// Full dump is strict; the delta relaxes the member filter under MSC4222.
	let after = state_after.requested();
	let state_events = current_state_ids
		.stream()
		.map_ok(|ids| (false, ids))
		.chain(
			state_diff_ids
				.stream()
				.map(move |ids| Ok((after, ids))),
		)
		.broad_and_then(async |(after, (shortstatekey, shorteventid))| {
			let event_id =
				lazy_filter(services, sender_user, witness, shortstatekey, shorteventid, after)
					.await;

			Ok(event_id)
		})
		.ready_try_filter_map(Result::Ok)
		.broad_and_then(|shorteventid| get_pdu(shorteventid).map(Ok))
		.ready_try_filter_map(Result::Ok)
		.try_collect::<Vec<_>>();

	let lazy_members = lazy_state_ids
		.stream()
		.broad_filter_map(get_pdu)
		.collect::<Vec<_>>()
		.map(Ok);

	let (state_events, lazy_members) = try_join(state_events, lazy_members).await?;

	let send_member_counts = state_events
		.iter()
		.chain(lazy_members.iter())
		.any(|event| *event.kind() == RoomMember);

	let member_counts = send_member_counts
		.then_async(|| calculate_counts(services, room_id, sender_user, include_heroes));

	let (joined_member_count, invited_member_count, heroes) =
		member_counts.await.unwrap_or((None, None, None));

	Ok(StateChanges {
		heroes,
		joined_member_count,
		invited_member_count,
		state_events,
		lazy_members,
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
	include_heroes: bool,
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
		.client
		.sync
		.calculate_heroes
		.and_is(include_heroes)
		.and_is(small_room)
		.then_async(|| calculate_heroes(services, room_id, sender_user));

	(Some(joined_member_count), Some(invited_member_count), heroes.await)
}

/// Selects up to five room members for the room summary.
///
/// The requesting user and members excluded by the hero filter are omitted.
pub async fn calculate_heroes(
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

	// The join read leads; a joined hero settles it before the invite read.
	let is_hero = services
		.state_cache
		.is_joined(user_id, room_id)
		.is_false()
		.and(services.state_cache.is_invited(user_id, room_id))
		.is_false();

	is_hero.await.then(|| user_id.to_owned())
}

pub(super) async fn typings_event_for_user(
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
