use ruma::UserId;
use tuwunel_api::router::{self, ClientIp, Ruma, RumaResponse, State};
use tuwunel_service::{Services, rooms::timeline::PdusIterItem};

pub(crate) mod account;
pub(crate) mod account_data;
pub(crate) mod alias;
pub(crate) mod appservice;
pub(crate) mod backup;
pub(crate) mod capabilities;
pub(crate) mod context;
pub(crate) mod dehydrated_device;
pub(crate) mod device;
pub(crate) mod directory;
pub(crate) mod events;
pub(crate) mod filter;
pub(crate) mod keys;
pub(crate) mod media;
pub(crate) mod media_legacy;
pub(crate) mod membership;
pub(crate) mod message;
pub(crate) mod notice;
pub(crate) mod openid;
pub(crate) mod presence;
pub(crate) mod profile;
pub(crate) mod push;
pub(crate) mod read_marker;
pub(crate) mod redact;
pub(crate) mod register;
pub(crate) mod relations;
pub(crate) mod rendezvous;
pub(crate) mod report;
pub(crate) mod room;
/// Provides the client route builder.
///
/// It assembles client endpoints using the server configuration.
pub mod routes;
pub(crate) mod rtc;
pub(crate) mod search;
pub(crate) mod send;
pub(crate) mod session;
pub(crate) mod space;
pub(crate) mod state;
pub(crate) mod sync;
pub(crate) mod tag;
pub(crate) mod thirdparty;
pub(crate) mod threads;
pub(crate) mod to_device;
pub(crate) mod tuwunel;
pub(crate) mod typing;
pub(crate) mod unstable;
pub(crate) mod user_directory;
pub(crate) mod versions;
pub(crate) mod voip;
pub(crate) mod well_known;

mod utils;

pub(crate) use self::{
	account::{
		add_3pid_route, change_password_route, deactivate_route, delete_3pid_route,
		get_email_validate_route, post_email_validate_route,
		request_3pid_management_token_via_email_route,
		request_3pid_management_token_via_msisdn_route,
		request_password_change_token_via_email_route,
		request_registration_token_via_email_route, third_party_route, whoami_route,
	},
	account_data::{
		delete_global_account_data_route, delete_room_account_data_route,
		get_global_account_data_route, get_room_account_data_route, is_empty_account_data_event,
		set_global_account_data_route, set_room_account_data_route,
	},
	alias::{create_alias_route, delete_alias_route, get_alias_route},
	appservice::appservice_ping,
	backup::{
		add_backup_keys_for_room_route, add_backup_keys_for_session_route, add_backup_keys_route,
		create_backup_version_route, delete_backup_keys_for_room_route,
		delete_backup_keys_for_session_route, delete_backup_keys_route,
		delete_backup_version_route, get_backup_info_route, get_backup_keys_for_room_route,
		get_backup_keys_for_session_route, get_backup_keys_route, get_latest_backup_info_route,
		update_backup_version_route,
	},
	capabilities::get_capabilities_route,
	context::get_context_route,
	dehydrated_device::{
		delete_dehydrated_device_route, get_dehydrated_device_route, get_dehydrated_events_route,
		put_dehydrated_device_route,
	},
	device::{
		delete_device_route, delete_devices_route, get_device_route, get_devices_route,
		update_device_route,
	},
	directory::{
		get_public_rooms_filtered_route, get_public_rooms_route, get_room_visibility_route,
		set_room_visibility_route,
	},
	events::events_route,
	filter::{create_filter_route, get_filter_route},
	keys::{
		claim_keys_route, get_key_changes_route, get_keys_route, upload_keys_route,
		upload_signatures_route, upload_signing_keys_route,
	},
	media::{
		create_content_async_route, create_content_route, create_mxc_uri_route,
		get_content_as_filename_route, get_content_route, get_content_thumbnail_route,
		get_media_config_route, get_media_preview_route,
	},
	media_legacy::{
		get_content_as_filename_legacy_route, get_content_legacy_route,
		get_content_thumbnail_legacy_route, get_media_config_legacy_route,
		get_media_preview_legacy_route,
	},
	membership::{
		ban_user_route, forget_room_route, get_member_events_route, invite_user_route,
		join_room_by_id_or_alias_route, join_room_by_id_route, joined_members_route,
		joined_rooms_route, kick_user_route, knock_room_route, leave_room_route,
		unban_user_route,
	},
	message::{annotate_membership, get_message_events_route, ignored_filter, is_ignored_pdu},
	openid::create_openid_token_route,
	presence::{get_presence_route, set_presence_route},
	profile::{
		delete_profile_field_route, get_profile_field_route, get_profile_route,
		set_profile_field_route,
	},
	push::{
		delete_pushrule_route, get_notifications_route, get_pushers_route,
		get_pushrule_actions_route, get_pushrule_enabled_route, get_pushrule_route,
		get_pushrules_all_route, get_pushrules_global_route, set_pushers_route,
		set_pushrule_actions_route, set_pushrule_enabled_route, set_pushrule_route,
	},
	read_marker::{create_receipt_route, set_read_marker_route},
	redact::redact_event_route,
	register::{check_registration_token_validity, get_register_available_route, register_route},
	relations::{
		get_relating_events_route, get_relating_events_with_rel_type_and_event_type_route,
		get_relating_events_with_rel_type_route,
	},
	rendezvous::{
		create_msc4388_route, create_rendezvous_route, delete_msc4388_route,
		delete_rendezvous_route, discover_msc4388_route, get_msc4388_route, get_rendezvous_route,
		put_msc4388_route, put_rendezvous_route,
	},
	report::{report_event_route, report_room_route, report_user_route},
	room::{
		create_room_route, get_event_by_timestamp_route, get_room_aliases_route,
		get_room_event_route, get_room_summary, get_room_summary_legacy, room_initial_sync_route,
		upgrade_room_route,
	},
	rtc::get_transports_route,
	search::search_events_route,
	send::send_message_event_route,
	session::{
		get_login_types_route, login_route, login_token_route, logout_all_route, logout_route,
		refresh_token_route, sso_callback_route, sso_complete_js_route, sso_css_route,
		sso_fallback_route, sso_login_route, sso_login_with_provider_route,
	},
	space::get_hierarchy_route,
	state::{
		get_state_events_for_empty_key_route, get_state_events_for_key_route,
		get_state_events_route, send_state_event_for_empty_key_route,
		send_state_event_for_key_route,
	},
	sync::{sync_events_route, sync_events_v5_route},
	tag::{delete_tag_route, get_tags_route, update_tag_route},
	thirdparty::{
		get_location_for_protocol_route, get_location_for_room_alias_route, get_protocol_route,
		get_protocols_route, get_user_for_protocol_route, get_user_for_user_id_route,
	},
	threads::get_threads_route,
	to_device::send_event_to_device_route,
	tuwunel::{tuwunel_local_user_count, tuwunel_remote_version, tuwunel_server_version},
	typing::create_typing_event_route,
	unstable::get_mutual_rooms_route,
	user_directory::search_users_route,
	versions::get_supported_versions_route,
	voip::turn_server_route,
	well_known::{well_known_client, well_known_support},
};
pub use self::{
	context::{ContextArgs, event_context},
	directory::get_public_rooms_filtered_helper,
	keys::{claim_keys_helper, get_keys_helper},
	message::{MessagesArgs, get_messages, with_membership},
	notice::{notice_tag, room_is_notice},
	space::{HierarchyArgs, get_client_hierarchy},
	sync::calculate_heroes,
};

/// generated user access token length
const TOKEN_LENGTH: usize = tuwunel_service::users::device::TOKEN_LENGTH;

/// generated user session ID length
const SESSION_ID_LENGTH: usize = tuwunel_service::uiaa::SESSION_ID_LENGTH;

/// Keeps a timeline item only when the user may see its event.
///
/// The room's history visibility is read as it stood at that event rather than
/// as it stands at the time of the request.
#[inline]
async fn visibility_filter(
	services: &Services,
	item: PdusIterItem,
	user_id: &UserId,
) -> Option<PdusIterItem> {
	let (_, pdu) = &item;

	services
		.state_accessor
		.user_can_see_event(user_id, pdu)
		.await
		.then_some(item)
}

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}
