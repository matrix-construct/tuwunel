//! Synapse admin API: room endpoints.

mod block;
mod context;
mod delete_room;
mod delete_status;
mod forward_extremities;
mod hierarchy;
mod join;
mod list_rooms;
mod make_room_admin;
mod messages;
mod purge_history;
mod room_details;
mod room_members;
mod state;
mod timestamp_to_event;

use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{join, join3, join5},
};
use ruma::{
	RoomId, UInt,
	events::{
		StateEventType,
		room::{
			guest_access::RoomGuestAccessEventContent,
			history_visibility::RoomHistoryVisibilityEventContent,
		},
	},
	uint,
};
use synapse_admin_api::rooms::list_rooms::v1::RoomDetails;
use tuwunel_core::{Result, matrix::Event, utils::TryFutureExtExt};
use tuwunel_service::Services;

pub(crate) use self::{
	block::{admin_get_room_block_route, admin_set_room_block_route},
	context::admin_room_context_route,
	delete_room::{admin_delete_room_v1_route, admin_delete_room_v2_route},
	delete_status::{admin_delete_status_by_id_route, admin_delete_status_by_room_route},
	forward_extremities::{
		admin_delete_forward_extremities_route, admin_get_forward_extremities_route,
	},
	hierarchy::admin_room_hierarchy_route,
	join::admin_join_room_route,
	list_rooms::admin_list_rooms_route,
	make_room_admin::admin_make_room_admin_route,
	messages::admin_room_messages_route,
	purge_history::{
		admin_purge_history_by_event_route, admin_purge_history_route,
		admin_purge_history_status_route,
	},
	room_details::admin_room_details_route,
	room_members::admin_room_members_route,
	state::admin_room_state_route,
	timestamp_to_event::admin_room_timestamp_to_event_route,
};

/// Action name recorded for the async room-shutdown task, mirroring Synapse's
/// `SHUTDOWN_AND_PURGE_ROOM`. The delete-status endpoints filter on it.
const DELETE_ROOM_ACTION: &str = "shutdown_and_purge_room";

/// Action name recorded for the history-purge task, filtered on by the
/// purge-status endpoint.
const PURGE_HISTORY_ACTION: &str = "purge_history";

/// Assembles the shared per-room summary row returned by both the room-list and
/// room-details endpoints.
async fn room_row(services: &Services, room_id: &RoomId) -> RoomDetails {
	let name = services.state_accessor.get_name(room_id).ok();

	let canonical_alias = services
		.state_accessor
		.get_canonical_alias(room_id)
		.ok();

	let room_type = services
		.state_accessor
		.get_room_type(room_id)
		.ok();

	let encryption = services
		.state_accessor
		.get_room_encryption(room_id)
		.map_ok(|algorithm| algorithm.to_string())
		.ok();

	let join_rules = services
		.state_accessor
		.get_join_rules(room_id)
		.map(|join_rule| Some(join_rule.kind()));

	let guest_access = services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomGuestAccess, "")
		.map_ok(|content: RoomGuestAccessEventContent| content.guest_access)
		.ok();

	let history_visibility = services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomHistoryVisibility, "")
		.map_ok(|content: RoomHistoryVisibilityEventContent| content.history_visibility)
		.ok();

	let create = services
		.state_accessor
		.get_create(room_id)
		.map(Result::ok);

	let version = services
		.state
		.get_room_version(room_id)
		.map_ok(|version| version.to_string())
		.ok();

	let public = services.directory.is_public_room(room_id);

	let joined_members = services
		.state_cache
		.room_joined_count(room_id)
		.map(|count| count_to_uint(count.ok()));

	let joined_local_members = services
		.state_cache
		.local_users_in_room(room_id)
		.count()
		.map(usize_to_uint);

	let state_events = state_event_count(services, room_id);

	let (
		(name, canonical_alias, room_type, encryption, join_rules),
		(guest_access, history_visibility, create, version, public),
		(joined_members, joined_local_members, state_events),
	) = join(
		join5(name, canonical_alias, room_type, encryption, join_rules),
		join(
			join5(guest_access, history_visibility, create, version, public),
			join3(joined_members, joined_local_members, state_events),
		),
	)
	.map(|(head, (mid, tail))| (head, mid, tail))
	.boxed()
	.await;

	let (creator, federatable) = create
		.map(|create| (Some(create.sender().to_owned()), create.federate().unwrap_or(true)))
		.unwrap_or((None, true));

	RoomDetails {
		room_id: room_id.to_owned(),
		name,
		canonical_alias,
		joined_members,
		joined_local_members,
		version,
		creator,
		encryption,
		federatable,
		public,
		join_rules,
		guest_access,
		history_visibility,
		state_events,
		room_type,
	}
}

async fn state_event_count(services: &Services, room_id: &RoomId) -> UInt {
	let Ok(shortstatehash) = services
		.state
		.get_room_shortstatehash(room_id)
		.await
	else {
		return uint!(0);
	};

	let count = services
		.state_accessor
		.state_full_ids(shortstatehash)
		.count()
		.await;

	usize_to_uint(count)
}

fn count_to_uint(count: Option<u64>) -> UInt {
	count
		.and_then(|count| count.try_into().ok())
		.unwrap_or_else(|| uint!(0))
}

fn usize_to_uint(count: usize) -> UInt { count.try_into().unwrap_or_else(|_| uint!(0)) }
