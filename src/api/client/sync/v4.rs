use std::{
	cmp::{self, Ordering},
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	time::Duration,
};

use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt};
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, RoomId, UInt, UserId,
	api::client::sync::sync_events::{
		self, DeviceLists, UnreadNotificationsCount,
		v4::{SlidingOp, SlidingSyncRoomHero},
	},
	directory::RoomTypeFilter,
	events::{
		AnyRawAccountDataEvent, AnySyncEphemeralRoomEvent, StateEventType,
		TimelineEventType::*,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	serde::Raw,
	uint,
};
use tuwunel_core::{
	Err, Error, PduCount, PduEvent, Result, debug, error, extract_variant,
	matrix::TypeStateKey,
	utils::{
		BoolExt, IterStream, ReadyExt, TryFutureExtExt,
		math::{ruma_from_usize, usize_from_ruma, usize_from_u64_truncated},
	},
	warn,
};
use tuwunel_service::{
	Services,
	rooms::read_receipt::pack_receipts,
	sync::{into_db_key, into_snake_key},
};

use super::{load_timeline, share_encrypted_room};
use crate::{
	Ruma,
	client::{DEFAULT_BUMP_TYPES, ignored_filter},
};

type TodoRooms = BTreeMap<OwnedRoomId, (BTreeSet<TypeStateKey>, usize, u64)>;
const SINGLE_CONNECTION_SYNC: &str = "single_connection_sync";

/// POST `/_matrix/client/unstable/org.matrix.msc3575/sync`
///
/// Sliding Sync endpoint (future endpoint: `/_matrix/client/v4/sync`)
pub(crate) async fn sync_events_v4_route(
	State(services): State<crate::State>,
	body: Ruma<sync_events::v4::Request>,
) -> Result<sync_events::v4::Response> {
	debug_assert!(DEFAULT_BUMP_TYPES.is_sorted(), "DEFAULT_BUMP_TYPES is not sorted");
	let sender_user = body
		.sender_user
		.as_ref()
		.expect("user is authenticated");
	let sender_device = body
		.sender_device
		.as_ref()
		.expect("user is authenticated");
	let mut body = body.body;

	// Setup watchers, so if there's no response, we can wait for them
	let watcher = services.sync.watch(sender_user, sender_device);

	let next_batch = services.globals.next_count()?;

	let conn_id = body
		.conn_id
		.clone()
		.unwrap_or_else(|| SINGLE_CONNECTION_SYNC.to_owned());

	let globalsince = body
		.pos
		.as_ref()
		.and_then(|string| string.parse().ok())
		.unwrap_or(0);

	let db_key = into_db_key(sender_user, sender_device, conn_id.clone());
	if globalsince != 0 && !services.sync.remembered(&db_key) {
		debug!("Restarting sync stream because it was gone from the database");
		return Err!(Request(UnknownPos("Connection data lost since last time")));
	}

	if globalsince == 0 {
		services
			.sync
			.forget_sync_request_connection(&db_key);
	}

	// Get sticky parameters from cache
	let snake_key = into_snake_key(sender_user, sender_device, conn_id.clone());
	let known_rooms = services
		.sync
		.update_sync_request_with_cache(&snake_key, &mut body);

	let all_joined_rooms: Vec<_> = services
		.rooms
		.state_cache
		.rooms_joined(sender_user)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let all_invited_rooms: Vec<_> = services
		.rooms
		.state_cache
		.rooms_invited(sender_user)
		.map(|r| r.0)
		.collect()
		.await;

	let all_knocked_rooms: Vec<_> = services
		.rooms
		.state_cache
		.rooms_knocked(sender_user)
		.map(|r| r.0)
		.collect()
		.await;

	let all_invited_rooms: Vec<&RoomId> = all_invited_rooms
		.iter()
		.map(AsRef::as_ref)
		.collect();
	let all_knocked_rooms: Vec<&RoomId> = all_knocked_rooms
		.iter()
		.map(AsRef::as_ref)
		.collect();

	let all_rooms: Vec<&RoomId> = all_joined_rooms
		.iter()
		.map(AsRef::as_ref)
		.chain(all_invited_rooms.iter().map(AsRef::as_ref))
		.chain(all_knocked_rooms.iter().map(AsRef::as_ref))
		.collect();

	let all_joined_rooms = all_joined_rooms
		.iter()
		.map(AsRef::as_ref)
		.collect();
	let all_invited_rooms = all_invited_rooms
		.iter()
		.map(AsRef::as_ref)
		.collect();

	if body.extensions.to_device.enabled.unwrap_or(false) {
		services
			.users
			.remove_to_device_events(sender_user, sender_device, globalsince)
			.await;
	}

	let mut left_encrypted_users = HashSet::new(); // Users that have left any encrypted rooms the sender was in
	let mut device_list_changes = HashSet::new();
	let mut device_list_left = HashSet::new();

	let mut receipts = sync_events::v4::Receipts { rooms: BTreeMap::new() };

	let mut account_data = sync_events::v4::AccountData {
		global: Vec::new(),
		rooms: BTreeMap::new(),
	};
	if body
		.extensions
		.account_data
		.enabled
		.unwrap_or(false)
	{
		account_data.global = services
			.account_data
			.changes_since(None, sender_user, globalsince, Some(next_batch))
			.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Global))
			.collect()
			.await;

		if let Some(rooms) = body.extensions.account_data.rooms {
			for room in rooms {
				account_data.rooms.insert(
					room.clone(),
					services
						.account_data
						.changes_since(Some(&room), sender_user, globalsince, Some(next_batch))
						.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Room))
						.collect()
						.await,
				);
			}
		}
	}

	if body.extensions.e2ee.enabled.unwrap_or(false) {
		// Look for device list updates of this account
		device_list_changes.extend(
			services
				.users
				.keys_changed(sender_user, globalsince, None)
				.map(ToOwned::to_owned)
				.collect::<Vec<_>>()
				.await,
		);

		for room_id in &all_joined_rooms {
			let room_id: &&RoomId = room_id;
			let Ok(current_shortstatehash) = services
				.rooms
				.state
				.get_room_shortstatehash(room_id)
				.await
			else {
				error!("Room {room_id} has no state");
				continue;
			};

			let since_shortstatehash = services
				.rooms
				.user
				.get_token_shortstatehash(room_id, globalsince)
				.await
				.ok();

			let encrypted_room = services
				.rooms
				.state_accessor
				.state_get(current_shortstatehash, &StateEventType::RoomEncryption, "")
				.await
				.is_ok();

			if let Some(since_shortstatehash) = since_shortstatehash {
				// Skip if there are only timeline changes
				if since_shortstatehash == current_shortstatehash {
					continue;
				}

				let since_encryption = services
					.rooms
					.state_accessor
					.state_get(since_shortstatehash, &StateEventType::RoomEncryption, "")
					.await;

				let since_sender_member: Option<RoomMemberEventContent> = services
					.rooms
					.state_accessor
					.state_get_content(
						since_shortstatehash,
						&StateEventType::RoomMember,
						sender_user.as_str(),
					)
					.ok()
					.await;

				let joined_since_last_sync = since_sender_member
					.as_ref()
					.is_none_or(|member| member.membership != MembershipState::Join);

				let new_encrypted_room = encrypted_room && since_encryption.is_err();

				if encrypted_room {
					let current_state_ids: HashMap<_, OwnedEventId> = services
						.rooms
						.state_accessor
						.state_full_ids(current_shortstatehash)
						.collect()
						.await;

					let since_state_ids: HashMap<_, _> = services
						.rooms
						.state_accessor
						.state_full_ids(since_shortstatehash)
						.collect()
						.await;

					for (key, id) in current_state_ids {
						if since_state_ids.get(&key) != Some(&id) {
							let Ok(pdu) = services.rooms.timeline.get_pdu(&id).await else {
								error!("Pdu in state not found: {id}");
								continue;
							};
							if pdu.kind == RoomMember {
								if let Some(Ok(user_id)) =
									pdu.state_key.as_deref().map(UserId::parse)
								{
									if user_id == sender_user {
										continue;
									}

									let content: RoomMemberEventContent = pdu.get_content()?;
									match content.membership {
										| MembershipState::Join => {
											// A new user joined an encrypted room
											if !share_encrypted_room(
												&services,
												sender_user,
												user_id,
												Some(room_id),
											)
											.await
											{
												device_list_changes.insert(user_id.to_owned());
											}
										},
										| MembershipState::Leave => {
											// Write down users that have left encrypted rooms we
											// are in
											left_encrypted_users.insert(user_id.to_owned());
										},
										| _ => {},
									}
								}
							}
						}
					}
					if joined_since_last_sync || new_encrypted_room {
						// If the user is in a new encrypted room, give them all joined users
						device_list_changes.extend(
							services
								.rooms
								.state_cache
								.room_members(room_id)
								// Don't send key updates from the sender to the sender
								.ready_filter(|&user_id| sender_user != user_id)
								// Only send keys if the sender doesn't share an encrypted room with the target
								// already
								.filter_map(|user_id| {
									share_encrypted_room(&services, sender_user, user_id, Some(room_id))
										.map(|res| res.or_some(user_id.to_owned()))
								})
								.collect::<Vec<_>>()
								.await,
						);
					}
				}
			}
			// Look for device list updates in this room
			device_list_changes.extend(
				services
					.users
					.room_keys_changed(room_id, globalsince, None)
					.map(|(user_id, _)| user_id)
					.map(ToOwned::to_owned)
					.collect::<Vec<_>>()
					.await,
			);
		}

		for user_id in left_encrypted_users {
			let dont_share_encrypted_room =
				!share_encrypted_room(&services, sender_user, &user_id, None).await;

			// If the user doesn't share an encrypted room with the target anymore, we need
			// to tell them
			if dont_share_encrypted_room {
				device_list_left.insert(user_id);
			}
		}
	}

	let mut lists = BTreeMap::new();
	let mut todo_rooms: TodoRooms = BTreeMap::new(); // and required state

	for (list_id, list) in &body.lists {
		let active_rooms = match list.filters.clone().and_then(|f| f.is_invite) {
			| Some(true) => &all_invited_rooms,
			| Some(false) => &all_joined_rooms,
			| None => &all_rooms,
		};

		let active_rooms = match list.filters.clone().map(|f| f.not_room_types) {
			| Some(filter) if filter.is_empty() => active_rooms.clone(),
			| Some(value) => filter_rooms(&services, active_rooms, &value, true).await,
			| None => active_rooms.clone(),
		};

		let active_rooms = match list.filters.clone().map(|f| f.room_types) {
			| Some(filter) if filter.is_empty() => active_rooms.clone(),
			| Some(value) => filter_rooms(&services, &active_rooms, &value, false).await,
			| None => active_rooms,
		};

		let mut new_known_rooms: BTreeSet<OwnedRoomId> = BTreeSet::new();

		let ranges = list.ranges.clone();
		lists.insert(
			list_id.clone(),
			sync_events::v4::SyncList {
				ops: ranges
					.into_iter()
					.map(|mut r| {
						r.0 = r.0.clamp(
							uint!(0),
							UInt::try_from(active_rooms.len().saturating_sub(1))
								.unwrap_or(UInt::MAX),
						);
						r.1 = r.1.clamp(
							r.0,
							UInt::try_from(active_rooms.len().saturating_sub(1))
								.unwrap_or(UInt::MAX),
						);

						let room_ids = if !active_rooms.is_empty() {
							active_rooms[usize_from_ruma(r.0)..=usize_from_ruma(r.1)].to_vec()
						} else {
							Vec::new()
						};

						new_known_rooms.extend(
							room_ids
								.clone()
								.into_iter()
								.map(ToOwned::to_owned),
						);
						for room_id in &room_ids {
							let todo_room = todo_rooms
								.entry((*room_id).to_owned())
								.or_insert((BTreeSet::new(), 0_usize, u64::MAX));

							let limit: usize = list
								.room_details
								.timeline_limit
								.map(u64::from)
								.map_or(10, usize_from_u64_truncated)
								.min(100);

							todo_room.0.extend(
								list.room_details
									.required_state
									.iter()
									.map(|(ty, sk)| (ty.clone(), sk.as_str().into())),
							);

							todo_room.1 = todo_room.1.max(limit);
							// 0 means unknown because it got out of date
							todo_room.2 = todo_room.2.min(
								known_rooms
									.get(list_id.as_str())
									.and_then(|k| k.get(*room_id))
									.copied()
									.unwrap_or(0),
							);
						}
						sync_events::v4::SyncOp {
							op: SlidingOp::Sync,
							range: Some(r),
							index: None,
							room_ids: room_ids
								.into_iter()
								.map(ToOwned::to_owned)
								.collect(),
							room_id: None,
						}
					})
					.collect(),
				count: ruma_from_usize(active_rooms.len()),
			},
		);

		if let Some(conn_id) = &body.conn_id {
			let db_key = into_db_key(sender_user, sender_device, conn_id);
			services.sync.update_sync_known_rooms(
				&db_key,
				list_id.clone(),
				new_known_rooms,
				globalsince,
			);
		}
	}

	let mut known_subscription_rooms = BTreeSet::new();
	for (room_id, room) in &body.room_subscriptions {
		if !services.rooms.metadata.exists(room_id).await
			|| services.rooms.metadata.is_disabled(room_id).await
			|| services.rooms.metadata.is_banned(room_id).await
		{
			continue;
		}
		let todo_room =
			todo_rooms
				.entry(room_id.clone())
				.or_insert((BTreeSet::new(), 0_usize, u64::MAX));

		let limit: usize = room
			.timeline_limit
			.map(u64::from)
			.map_or(10, usize_from_u64_truncated)
			.min(100);

		todo_room.0.extend(
			room.required_state
				.iter()
				.map(|(ty, sk)| (ty.clone(), sk.as_str().into())),
		);
		todo_room.1 = todo_room.1.max(limit);
		// 0 means unknown because it got out of date
		todo_room.2 = todo_room.2.min(
			known_rooms
				.get("subscriptions")
				.and_then(|k| k.get(room_id))
				.copied()
				.unwrap_or(0),
		);
		known_subscription_rooms.insert(room_id.clone());
	}

	for r in body.unsubscribe_rooms {
		known_subscription_rooms.remove(&r);
		body.room_subscriptions.remove(&r);
	}

	if let Some(conn_id) = &body.conn_id {
		let db_key = into_db_key(sender_user, sender_device, conn_id);
		services.sync.update_sync_known_rooms(
			&db_key,
			"subscriptions".to_owned(),
			known_subscription_rooms,
			globalsince,
		);
	}

	if let Some(conn_id) = body.conn_id.clone() {
		let db_key = into_db_key(sender_user, sender_device, conn_id);
		services
			.sync
			.update_sync_subscriptions(&db_key, body.room_subscriptions);
	}

	let mut rooms = BTreeMap::new();
	for (room_id, (required_state_request, timeline_limit, roomsince)) in &todo_rooms {
		let roomsincecount = PduCount::Normal(*roomsince);

		let mut timestamp: Option<_> = None;
		let mut invite_state = None;
		let (timeline_pdus, limited);
		let new_room_id: &RoomId = (*room_id).as_ref();
		if all_invited_rooms.contains(&new_room_id) {
			// TODO: figure out a timestamp we can use for remote invites
			invite_state = services
				.rooms
				.state_cache
				.invite_state(sender_user, room_id)
				.await
				.ok();

			(timeline_pdus, limited) = (Vec::new(), true);
		} else {
			(timeline_pdus, limited) = match load_timeline(
				&services,
				sender_user,
				room_id,
				roomsincecount,
				None,
				*timeline_limit,
			)
			.await
			{
				| Ok(value) => value,
				| Err(err) => {
					warn!("Encountered missing timeline in {}, error {}", room_id, err);
					continue;
				},
			};
		}

		account_data.rooms.insert(
			room_id.to_owned(),
			services
				.account_data
				.changes_since(Some(room_id), sender_user, *roomsince, Some(next_batch))
				.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Room))
				.collect()
				.await,
		);

		let last_privateread_update = services
			.rooms
			.read_receipt
			.last_privateread_update(sender_user, room_id)
			.await > *roomsince;

		let private_read_event = if last_privateread_update {
			services
				.rooms
				.read_receipt
				.private_read_get(room_id, sender_user)
				.await
				.ok()
		} else {
			None
		};

		let mut vector: Vec<Raw<AnySyncEphemeralRoomEvent>> = services
			.rooms
			.read_receipt
			.readreceipts_since(room_id, *roomsince)
			.filter_map(|(read_user, _ts, v)| async move {
				services
					.users
					.user_is_ignored(read_user, sender_user)
					.await
					.or_some(v)
			})
			.collect()
			.await;

		if let Some(private_read_event) = private_read_event {
			vector.push(private_read_event);
		}

		let receipt_size = vector.len();
		receipts
			.rooms
			.insert(room_id.clone(), pack_receipts(Box::new(vector.into_iter())));

		if roomsince != &0
			&& timeline_pdus.is_empty()
			&& account_data
				.rooms
				.get(room_id)
				.is_some_and(Vec::is_empty)
			&& receipt_size == 0
		{
			continue;
		}

		let prev_batch = timeline_pdus
			.first()
			.map_or(Ok::<_, Error>(None), |(pdu_count, _)| {
				Ok(Some(match pdu_count {
					| PduCount::Backfilled(_) => {
						error!("timeline in backfill state?!");
						"0".to_owned()
					},
					| PduCount::Normal(c) => c.to_string(),
				}))
			})?
			.or_else(|| {
				if roomsince != &0 {
					Some(roomsince.to_string())
				} else {
					None
				}
			});

		let room_events: Vec<_> = timeline_pdus
			.iter()
			.stream()
			.filter_map(|item| ignored_filter(&services, item.clone(), sender_user))
			.map(|(_, pdu)| pdu.to_sync_room_event())
			.collect()
			.await;

		for (_, pdu) in timeline_pdus {
			let ts = MilliSecondsSinceUnixEpoch(pdu.origin_server_ts);
			if DEFAULT_BUMP_TYPES
				.binary_search(&pdu.kind)
				.is_ok() && timestamp.is_none_or(|time| time <= ts)
			{
				timestamp = Some(ts);
			}
		}

		let required_state = required_state_request
			.iter()
			.stream()
			.filter_map(|state| async move {
				services
					.rooms
					.state_accessor
					.room_state_get(room_id, &state.0, &state.1)
					.await
					.map(PduEvent::into_sync_state_event)
					.ok()
			})
			.collect()
			.await;

		// Heroes
		let heroes: Vec<_> = services
			.rooms
			.state_cache
			.room_members(room_id)
			.ready_filter(|&member| member != sender_user)
			.filter_map(|user_id| {
				services
					.rooms
					.state_accessor
					.get_member(room_id, user_id)
					.map_ok(|memberevent| SlidingSyncRoomHero {
						user_id: user_id.into(),
						name: memberevent.displayname,
						avatar: memberevent.avatar_url,
					})
					.ok()
			})
			.take(5)
			.collect()
			.await;

		let name = match heroes.len().cmp(&(1_usize)) {
			| Ordering::Greater => {
				let firsts = heroes[1..]
					.iter()
					.map(|h| {
						h.name
							.clone()
							.unwrap_or_else(|| h.user_id.to_string())
					})
					.collect::<Vec<_>>()
					.join(", ");

				let last = heroes[0]
					.name
					.clone()
					.unwrap_or_else(|| heroes[0].user_id.to_string());

				Some(format!("{firsts} and {last}"))
			},
			| Ordering::Equal => Some(
				heroes[0]
					.name
					.clone()
					.unwrap_or_else(|| heroes[0].user_id.to_string()),
			),
			| Ordering::Less => None,
		};

		let heroes_avatar = if heroes.len() == 1 {
			heroes[0].avatar.clone()
		} else {
			None
		};

		rooms.insert(
			room_id.clone(),
			sync_events::v4::SlidingSyncRoom {
				name: services
					.rooms
					.state_accessor
					.get_name(room_id)
					.await
					.ok()
					.or(name),
				avatar: match heroes_avatar {
					| Some(heroes_avatar) => ruma::JsOption::Some(heroes_avatar),
					| _ => match services
						.rooms
						.state_accessor
						.get_avatar(room_id)
						.await
					{
						| ruma::JsOption::Some(avatar) => ruma::JsOption::from_option(avatar.url),
						| ruma::JsOption::Null => ruma::JsOption::Null,
						| ruma::JsOption::Undefined => ruma::JsOption::Undefined,
					},
				},
				initial: Some(roomsince == &0),
				is_dm: None,
				invite_state,
				unread_notifications: UnreadNotificationsCount {
					highlight_count: Some(
						services
							.rooms
							.user
							.highlight_count(sender_user, room_id)
							.await
							.try_into()
							.expect("notification count can't go that high"),
					),
					notification_count: Some(
						services
							.rooms
							.user
							.notification_count(sender_user, room_id)
							.await
							.try_into()
							.expect("notification count can't go that high"),
					),
				},
				timeline: room_events,
				required_state,
				prev_batch,
				limited,
				joined_count: Some(
					services
						.rooms
						.state_cache
						.room_joined_count(room_id)
						.await
						.unwrap_or(0)
						.try_into()
						.unwrap_or_else(|_| uint!(0)),
				),
				invited_count: Some(
					services
						.rooms
						.state_cache
						.room_invited_count(room_id)
						.await
						.unwrap_or(0)
						.try_into()
						.unwrap_or_else(|_| uint!(0)),
				),
				num_live: None, // Count events in timeline greater than global sync counter
				timestamp,
				heroes: Some(heroes),
			},
		);
	}

	if rooms.iter().all(|(id, r)| {
		r.timeline.is_empty() && r.required_state.is_empty() && !receipts.rooms.contains_key(id)
	}) {
		// Hang a few seconds so requests are not spammed
		// Stop hanging if new info arrives
		let default = Duration::from_secs(30);
		let duration = cmp::min(body.timeout.unwrap_or(default), default);
		_ = tokio::time::timeout(duration, watcher).await;
	}

	Ok(sync_events::v4::Response {
		initial: globalsince == 0,
		txn_id: body.txn_id.clone(),
		pos: next_batch.to_string(),
		lists,
		rooms,
		extensions: sync_events::v4::Extensions {
			to_device: if body.extensions.to_device.enabled.unwrap_or(false) {
				Some(sync_events::v4::ToDevice {
					events: services
						.users
						.get_to_device_events(
							sender_user,
							sender_device,
							Some(globalsince),
							Some(next_batch),
						)
						.collect()
						.await,
					next_batch: next_batch.to_string(),
				})
			} else {
				None
			},
			e2ee: sync_events::v4::E2EE {
				device_lists: DeviceLists {
					changed: device_list_changes.into_iter().collect(),
					left: device_list_left.into_iter().collect(),
				},
				device_one_time_keys_count: services
					.users
					.count_one_time_keys(sender_user, sender_device)
					.await,
				// Fallback keys are not yet supported
				device_unused_fallback_key_types: None,
			},
			account_data,
			receipts,
			typing: sync_events::v4::Typing { rooms: BTreeMap::new() },
		},
		delta_token: None,
	})
}

async fn filter_rooms<'a>(
	services: &Services,
	rooms: &[&'a RoomId],
	filter: &[RoomTypeFilter],
	negate: bool,
) -> Vec<&'a RoomId> {
	rooms
		.iter()
		.stream()
		.filter_map(|r| async move {
			let room_type = services
				.rooms
				.state_accessor
				.get_room_type(r)
				.await;

			if room_type
				.as_ref()
				.is_err_and(|e| !e.is_not_found())
			{
				return None;
			}

			let room_type_filter = RoomTypeFilter::from(room_type.ok());

			let include = if negate {
				!filter.contains(&room_type_filter)
			} else {
				filter.is_empty() || filter.contains(&room_type_filter)
			};

			include.then_some(r)
		})
		.collect()
		.await
}
