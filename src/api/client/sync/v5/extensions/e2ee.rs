use std::collections::HashSet;

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, join4},
	stream::once,
};
use ruma::{
	OwnedUserId, RoomId,
	api::client::sync::sync_events::{DeviceLists, v5::response::E2EE},
	events::{
		StateEventType, TimelineEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use tuwunel_core::{
	Result, error, pair_of,
	utils::{
		BoolExt, FutureBoolExt, IterStream, ReadyExt, TryFutureExtExt,
		future::{OptionFutureExt, OptionStream, ReadyBoolExt},
		stream::BroadbandExt,
	},
};
use tuwunel_matrix::{Event, pdu::PduCount};
use tuwunel_service::sync::Connection;

use super::{SyncInfo, joined_since, share_encrypted_room};

#[tracing::instrument(name = "e2ee", level = "trace", skip_all)]
pub(super) async fn collect(sync_info: SyncInfo<'_>, conn: &Connection) -> Result<(E2EE, bool)> {
	let SyncInfo { services, sender_user, sender_device, .. } = sync_info;
	let Some(sender_device) = sender_device else {
		return Ok(Default::default());
	};

	let device_lists = sync_info
		.previous_connection_pos
		.is_some()
		.then_async(|| device_lists(sync_info, conn))
		.map(Option::unwrap_or_default);

	let keys_changed = services
		.users
		.last_one_time_keys_update(sender_user)
		.map(|since| since > conn.globalsince);

	let device_one_time_keys_count = services
		.users
		.count_one_time_keys(sender_user, sender_device);

	let device_unused_fallback_key_types = services
		.users
		.unused_fallback_key_algorithms(sender_user, sender_device)
		.collect::<Vec<_>>()
		.map(Some);

	let (
		device_lists,
		keys_changed,
		device_one_time_keys_count,
		device_unused_fallback_key_types,
	) = join4(
		device_lists,
		keys_changed,
		device_one_time_keys_count,
		device_unused_fallback_key_types,
	)
	.boxed() // size firewall
	.await;

	let response = E2EE {
		device_lists,
		device_one_time_keys_count,
		device_unused_fallback_key_types,
	};

	Ok((response, keys_changed))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn device_lists(sync_info: SyncInfo<'_>, conn: &Connection) -> DeviceLists {
	let SyncInfo { services, sender_user, .. } = sync_info;
	let keys_changed = services
		.users
		.keys_changed(sender_user, conn.globalsince, Some(conn.next_batch))
		.map(ToOwned::to_owned)
		.collect::<HashSet<_>>()
		.map(|changed| (changed, HashSet::new()));

	let (changed, left) = (HashSet::new(), HashSet::new());
	let (changed, left) = services
		.state_cache
		.rooms_joined(sender_user)
		.map(ToOwned::to_owned)
		.broad_filter_map(async |room_id| collect_room(sync_info, conn, &room_id).await.ok())
		.chain(once(keys_changed))
		.ready_fold((changed, left), |(mut changed, mut left), room| {
			changed.extend(room.0);
			left.extend(room.1);
			(changed, left)
		})
		.await;

	let left = left
		.into_iter()
		.stream()
		.broad_filter_map(async |user_id| {
			share_encrypted_room(services, sender_user, &user_id)
				.await
				.is_false()
				.then_some(user_id)
		})
		.collect()
		.await;

	DeviceLists {
		changed: changed.into_iter().collect(),
		left,
	}
}

#[tracing::instrument(level = "trace", skip_all, fields(room_id), ret)]
async fn collect_room(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	room_id: &RoomId,
) -> Result<pair_of!(HashSet<OwnedUserId>)> {
	let current_shortstatehash = services
		.state
		.get_room_shortstatehash(room_id)
		.inspect_err(|e| error!("Room {room_id} has no state: {e}"));

	let room_keys_changed = services
		.users
		.room_keys_changed(room_id, conn.globalsince, Some(conn.next_batch))
		.map(|(user_id, _)| user_id)
		.map(ToOwned::to_owned)
		.collect::<HashSet<_>>();

	let (current_shortstatehash, device_list_changed) =
		join(current_shortstatehash, room_keys_changed)
			.boxed() // erase region
			.await;

	let lists = (device_list_changed, HashSet::new());
	let Ok(current_shortstatehash) = current_shortstatehash else {
		return Ok(lists);
	};

	if current_shortstatehash <= conn.globalsince {
		return Ok(lists);
	}

	let Ok(since_shortstatehash) = services
		.timeline
		.prev_shortstatehash(room_id, PduCount::Normal(conn.globalsince).saturating_add(1))
		.await
	else {
		return Ok(lists);
	};

	if since_shortstatehash == current_shortstatehash {
		return Ok(lists);
	}

	let encrypted_at = |shortstatehash| {
		services
			.state_accessor
			.state_get_shortid(shortstatehash, &StateEventType::RoomEncryption, "")
			.is_ok()
	};

	let current_encrypted = encrypted_at(current_shortstatehash).await;

	if !current_encrypted
		&& services
			.config
			.encryption
			.device_key_update_encrypted_rooms_only
	{
		return Ok(lists);
	}

	let joined_since_last_sync = joined_since(services, room_id, sender_user, conn.globalsince);

	// A plaintext room bursts only on the sender's own join.
	let newly_encrypted = current_encrypted
		.then_async(|| encrypted_at(since_shortstatehash).is_false())
		.unwrap_or_default();

	// The join check leads; the state lookup trails.
	let members_burst = joined_since_last_sync
		.is_false()
		.and(newly_encrypted.is_false())
		.is_false()
		.await;

	let joined_members_burst = members_burst.then_async(|| {
		services
			.state_cache
			.room_members(room_id)
			.ready_filter(|&user_id| user_id != sender_user)
			.map(ToOwned::to_owned)
			.map(|user_id| (MembershipState::Join, user_id))
			.boxed() // Unpin for into_future
			.into_future()
	});

	services
		.state_accessor
		.state_added((since_shortstatehash, current_shortstatehash))
		.broad_filter_map(async |(_shortstatekey, shorteventid)| {
			services
				.timeline
				.get_pdu_from_shorteventid(shorteventid)
				.ok()
				.await
		})
		.ready_filter(|event| *event.kind() == TimelineEventType::RoomMember)
		.ready_filter(|event| {
			event
				.state_key()
				.is_some_and(|state_key| state_key != sender_user)
		})
		.ready_filter_map(|event| {
			let content: RoomMemberEventContent = event.get_content().ok()?;
			let user_id: OwnedUserId = event.state_key()?.parse().ok()?;

			Some((content.membership, user_id))
		})
		.chain(joined_members_burst.stream())
		.ready_fold(lists, |(mut changed, mut left), (membership, user_id)| {
			use MembershipState::*;

			match membership {
				| Join => changed.insert(user_id),
				| Leave => left.insert(user_id),
				| _ => false,
			};

			(changed, left)
		})
		.map(Ok)
		.await
}
