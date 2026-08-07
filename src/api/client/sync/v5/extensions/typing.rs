use std::collections::BTreeMap;

use futures::{FutureExt, StreamExt, TryFutureExt};
use ruma::{
	OwnedRoomId,
	api::client::sync::sync_events::v5::response::{self, Typing},
	events::typing::{SyncTypingEvent, TypingEventContent},
	serde::Raw,
};
use tuwunel_core::{
	Result, debug_error,
	smallvec::SmallVec,
	utils::{IterStream, stream::BroadbandExt},
};

use super::{Connection, SyncInfo, Window, selector};

type CollectedRooms = SmallVec<[(OwnedRoomId, CollectedRoom); 1]>;

#[derive(Debug, Default)]
pub(super) struct Collected {
	rooms: CollectedRooms,
}

#[derive(Debug)]
pub(super) struct CollectedRoom {
	event: Raw<SyncTypingEvent>,
	initial_only: bool,
}

impl Collected {
	pub(super) fn into_response(
		self,
		payloads: &BTreeMap<OwnedRoomId, response::Room>,
	) -> Typing {
		let rooms = self
			.rooms
			.into_iter()
			.filter_map(|(room_id, room)| {
				if room.initial_only
					&& payloads
						.get(&room_id)
						.is_none_or(|payload| payload.initial != Some(true))
				{
					return None;
				}

				Some((room_id, room.event))
			})
			.collect();

		Typing { rooms }
	}
}

#[tracing::instrument(name = "typing", level = "trace", skip_all, ret)]
pub(super) async fn collect(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
) -> Result<Collected> {
	let SyncInfo { services, sender_user, .. } = sync_info;

	let implicit = conn
		.extensions
		.typing
		.lists
		.as_deref()
		.map(<[_]>::iter);

	let explicit = conn
		.extensions
		.typing
		.rooms
		.as_deref()
		.map(<[_]>::iter);

	selector(conn, window, implicit, explicit)
		.stream()
		.broad_filter_map(async |room_id| {
			let roomsince = conn
				.rooms
				.get(room_id)
				.map(|room| room.roomsince)
				.unwrap_or_default();

			let eligible = move |update_token, has_users| {
				eligibility(update_token, conn.globalsince, conn.next_batch, roomsince, has_users)
			};

			let (update_token, users) = services
				.typing
				.typing_snapshot_for_user(room_id, sender_user, |update_token| {
					eligible(update_token, true).is_some()
				})
				.inspect_err(|e| debug_error!(%room_id, "Failed to get typing events: {e}"))
				.await
				.ok()??;

			let initial_only = eligible(update_token, !users.is_empty())?;

			let content = TypingEventContent::new(users);
			let event = SyncTypingEvent { content };
			let event = Raw::new(&event);

			event
				.ok()
				.map(|event| (room_id.to_owned(), CollectedRoom { event, initial_only }))
		})
		.collect::<CollectedRooms>()
		.map(|rooms| Collected { rooms })
		.map(Ok)
		.await
}

/// Whether a typing update publishes, and if so whether only alongside an
/// initial room payload.
///
/// `None` skips the room; `Some(false)` is a live update, empty clears
/// included; `Some(true)` is an initial candidate gated later on an actual
/// initial payload.
fn eligibility(
	update_token: u64,
	globalsince: u64,
	next_batch: u64,
	roomsince: u64,
	has_users: bool,
) -> Option<bool> {
	if update_token > next_batch {
		return None;
	}

	if globalsince < update_token {
		return Some(false);
	}

	(roomsince == 0 && has_users).then_some(true)
}

#[cfg(test)]
mod tests {
	use ruma::{
		OwnedUserId, RoomId, api::client::sync::sync_events::v5::response::Room as ResponseRoom,
		room_id, user_id,
	};

	use super::*;

	#[test]
	fn typing_update_gate_is_live_once_and_replays() {
		let live = eligibility(5, 4, 6, 6, true);
		let advanced = eligibility(5, 5, 7, 6, true);
		let replayed = eligibility(5, 4, 6, 6, true);

		assert_eq!(live, Some(false));
		assert_eq!(advanced, None);
		assert_eq!(replayed, Some(false));
	}

	#[test]
	fn future_and_unchanged_typing_updates_are_omitted() {
		assert_eq!(eligibility(7, 0, 6, 0, true), None);
		assert_eq!(eligibility(4, 5, 6, 3, true), None);
		assert_eq!(eligibility(0, 0, 6, 0, false), None);
	}

	#[test]
	fn empty_live_typing_is_preserved() {
		let room_id = room_id!("!typing-clear:example.com");
		let initial_only =
			eligibility(5, 4, 6, 6, false).expect("live empty typing should be eligible");

		let typing = collected(room_id, Vec::new(), initial_only).into_response(&BTreeMap::new());
		let event = typing.rooms[room_id]
			.deserialize()
			.expect("typing event should deserialize");

		assert!(event.content.user_ids.is_empty());
	}

	#[test]
	fn initial_typing_requires_an_actual_initial_payload() {
		let room_id = room_id!("!typing-initial:example.com");
		let users = vec![user_id!("@typing:example.com").to_owned()];
		let initial_only =
			eligibility(0, 0, 6, 0, true).expect("nonempty initial typing should be eligible");

		let missing =
			collected(room_id, users.clone(), initial_only).into_response(&BTreeMap::new());

		assert!(missing.rooms.is_empty());

		let payloads = [(room_id.to_owned(), ResponseRoom::default())].into();
		let noninitial = collected(room_id, users.clone(), initial_only).into_response(&payloads);
		assert!(noninitial.rooms.is_empty());

		let payload = ResponseRoom {
			initial: Some(true),
			..Default::default()
		};
		let payloads = [(room_id.to_owned(), payload)].into();
		let initial = collected(room_id, users.clone(), initial_only).into_response(&payloads);

		assert!(initial.rooms.contains_key(room_id));

		let live = collected(room_id, users, false).into_response(&BTreeMap::new());
		assert!(live.rooms.contains_key(room_id));
	}

	fn collected(room_id: &RoomId, users: Vec<OwnedUserId>, initial_only: bool) -> Collected {
		let content = TypingEventContent::new(users);
		let event = SyncTypingEvent { content };
		let event = Raw::new(&event).expect("typing event should serialize");
		let rooms = [(room_id.to_owned(), CollectedRoom { event, initial_only })].into();

		Collected { rooms }
	}
}
