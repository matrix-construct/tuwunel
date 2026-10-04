use tuwunel_api::router::{self, ClientIp, Ruma, State};

pub(crate) mod backfill;
pub(crate) mod event;
pub(crate) mod event_auth;
pub(crate) mod get_missing_events;
pub(crate) mod hierarchy;
pub(crate) mod invite;
pub(crate) mod key;
pub(crate) mod make_join;
pub(crate) mod make_knock;
pub(crate) mod make_leave;
pub(crate) mod media;
pub(crate) mod openid;
pub(crate) mod publicrooms;
pub(crate) mod query;
/// Provides the server route builder.
///
/// It includes discovery endpoints and applies the federation setting.
pub mod routes;
pub(crate) mod send;
pub(crate) mod send_join;
pub(crate) mod send_knock;
pub(crate) mod send_leave;
pub(crate) mod state;
pub(crate) mod state_ids;
pub(crate) mod timestamp;
pub(crate) mod user;
pub(crate) mod version;
pub(crate) mod well_known;

pub(crate) use self::{
	backfill::get_backfill_route,
	event::get_event_route,
	event_auth::get_event_authorization_route,
	get_missing_events::get_missing_events_route,
	hierarchy::get_hierarchy_route,
	invite::create_invite_route,
	key::get_server_keys_route,
	make_join::{create_join_event_template_route, user_can_perform_restricted_join},
	make_knock::create_knock_event_template_route,
	make_leave::create_leave_event_template_route,
	media::{get_content_route, get_content_thumbnail_route},
	openid::get_openid_userinfo_route,
	publicrooms::{get_public_rooms_filtered_route, get_public_rooms_route},
	query::{get_profile_information_route, get_room_information_route},
	send::send_transaction_message_route,
	send_join::create_join_event_v2_route,
	send_knock::create_knock_event_v1_route,
	send_leave::create_leave_event_v2_route,
	state::get_room_state_route,
	state_ids::get_room_state_ids_route,
	timestamp::get_event_by_timestamp_route,
	user::{claim_keys_route, get_devices_route, get_keys_route},
	version::get_server_version_route,
	well_known::well_known_server,
};

mod utils;
use utils::AccessCheck;

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}
