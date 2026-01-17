use std::collections::HashSet;

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, join3},
	stream::once,
};
use ruma::{
	OwnedUserId, RoomId,
	api::client::sync::sync_events::{DeviceLists, v5::response},
	events::{
		StateEventType, TimelineEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use tuwunel_core::{
	Result, error,
	matrix::{Event, pdu::PduCount},
	pair_of,
	utils::{
		BoolExt, IterStream, ReadyExt, TryFutureExtExt, future::OptionStream,
		stream::BroadbandExt,
	},
};
use tuwunel_service::sync::Connection;

use super::{SyncInfo, share_encrypted_room};

#[tracing::instrument(name = "e2ee", level = "trace", skip_all)]
pub(super) async fn collect(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
) -> Result<response::E2EE> {
	let SyncInfo { services, sender_user, sender_device, .. } = sync_info;
	let Some(sender_device) = sender_device else {
		return Ok(response::E2EE::default());
	};

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
		.filter_map(async |user_id| {
			share_encrypted_room(services, sender_user, &user_id, None)
				.await
				.is_false()
				.then_some(user_id)
		})
		.collect();

	let device_one_time_keys_count = services
		.users
		.last_one_time_keys_update(sender_user)
		.then(|since| {
			since.gt(&conn.globalsince).then_async(|| {
				services
					.users
					.count_one_time_keys(sender_user, sender_device)
			})
		})
		.map(Option::unwrap_or_default);

	let (left, device_one_time_keys_count) = join(left, device_one_time_keys_count)
		.boxed()
		.await;

	Ok(response::E2EE {
		device_one_time_keys_count,
		device_unused_fallback_key_types: None,
		device_lists: DeviceLists {
			changed: changed.into_iter().collect(),
			left,
		},
	})
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
			.boxed()
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

	let encrypted_room = services
		.state_accessor
		.state_get(current_shortstatehash, &StateEventType::RoomEncryption, "")
		.is_ok();

	let since_encryption = services
		.state_accessor
		.state_get(since_shortstatehash, &StateEventType::RoomEncryption, "")
		.is_ok();

	let sender_joined_count = services
		.state_cache
		.get_joined_count(room_id, sender_user);

	let (encrypted_room, since_encryption, sender_joined_count) =
		join3(encrypted_room, since_encryption, sender_joined_count).await;

	if !encrypted_room {
		return Ok(lists);
	}

	let encrypted_since_last_sync = !since_encryption;
	let joined_since_last_sync = sender_joined_count.is_ok_and(|count| count > conn.globalsince);
	let joined_members_burst =
		(joined_since_last_sync || encrypted_since_last_sync).then_async(|| {
			services
				.state_cache
				.room_members(room_id)
				.ready_filter(|&user_id| user_id != sender_user)
				.map(ToOwned::to_owned)
				.map(|user_id| (MembershipState::Join, user_id))
				.boxed()
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
		.fold(lists, async |(mut changed, mut left), (membership, user_id)| {
			use MembershipState::*;

			let should_add = async |user_id| {
				!share_encrypted_room(services, sender_user, user_id, Some(room_id)).await
			};

			match membership {
				| Join if should_add(&user_id).await => changed.insert(user_id),
				| Leave => left.insert(user_id),
				| _ => false,
			};

			(changed, left)
		})
		.map(Ok)
		.boxed()
		.await
}
