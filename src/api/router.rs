mod args;
mod auth;
mod handler;
mod request;
mod response;
pub mod state;

use std::str::FromStr;

use axum::{
	Router,
	response::{IntoResponse, Redirect},
	routing::{any, get, post},
};
use http::{Uri, uri};
use tuwunel_core::{Server, err};

use self::handler::RouterExt;
pub(super) use self::{
	args::Args as Ruma, auth::auth_uiaa, response::RumaResponse, state::State,
};
use crate::{client, server};

pub fn build(router: Router<State>, server: &Server) -> Router<State> {
	let config = &server.config;
	let mut router = router
        .ruma_route(&client::get_timezone_key_route)
        .ruma_route(&client::get_profile_field_route)
        .ruma_route(&client::set_profile_field_route)
        .ruma_route(&client::delete_profile_field_route)
        .ruma_route(&client::set_timezone_key_route)
        .ruma_route(&client::delete_timezone_key_route)
        .ruma_route(&client::appservice_ping)
		.ruma_route(&client::get_supported_versions_route)
		.ruma_route(&client::get_register_available_route)
		.ruma_route(&client::register_route)
		.ruma_route(&client::get_login_types_route)
		.ruma_route(&client::login_route)
		.ruma_route(&client::login_token_route)
		.ruma_route(&client::refresh_token_route)
		.ruma_route(&client::sso_login_route)
		.ruma_route(&client::sso_login_with_provider_route)
		.ruma_route(&client::sso_callback_route)
		.ruma_route(&client::whoami_route)
		.ruma_route(&client::logout_route)
		.ruma_route(&client::logout_all_route)
		.ruma_route(&client::change_password_route)
		.ruma_route(&client::deactivate_route)
		.ruma_route(&client::third_party_route)
		.ruma_route(&client::request_3pid_management_token_via_email_route)
		.ruma_route(&client::request_3pid_management_token_via_msisdn_route)
		.ruma_route(&client::check_registration_token_validity)
		.ruma_route(&client::get_notifications_route)
		.ruma_route(&client::get_capabilities_route)
		.ruma_route(&client::get_pushrules_all_route)
		.ruma_route(&client::get_pushrules_global_route)
		.ruma_route(&client::set_pushrule_route)
		.ruma_route(&client::get_pushrule_route)
		.ruma_route(&client::set_pushrule_enabled_route)
		.ruma_route(&client::get_pushrule_enabled_route)
		.ruma_route(&client::get_pushrule_actions_route)
		.ruma_route(&client::set_pushrule_actions_route)
		.ruma_route(&client::delete_pushrule_route)
		.ruma_route(&client::get_room_event_route)
		.ruma_route(&client::get_room_aliases_route)
		.ruma_route(&client::get_filter_route)
		.ruma_route(&client::create_filter_route)
		.ruma_route(&client::create_openid_token_route)
		.ruma_route(&client::set_global_account_data_route)
		.ruma_route(&client::set_room_account_data_route)
		.ruma_route(&client::get_global_account_data_route)
		.ruma_route(&client::get_room_account_data_route)
		.ruma_route(&client::set_displayname_route)
		.ruma_route(&client::get_displayname_route)
		.ruma_route(&client::set_avatar_url_route)
		.ruma_route(&client::get_avatar_url_route)
		.ruma_route(&client::get_profile_route)
		.ruma_route(&client::set_presence_route)
		.ruma_route(&client::get_presence_route)
		.ruma_route(&client::upload_keys_route)
		.ruma_route(&client::get_keys_route)
		.ruma_route(&client::claim_keys_route)
		.ruma_route(&client::create_backup_version_route)
		.ruma_route(&client::update_backup_version_route)
		.ruma_route(&client::delete_backup_version_route)
		.ruma_route(&client::get_latest_backup_info_route)
		.ruma_route(&client::get_backup_info_route)
		.ruma_route(&client::add_backup_keys_route)
		.ruma_route(&client::add_backup_keys_for_room_route)
		.ruma_route(&client::add_backup_keys_for_session_route)
		.ruma_route(&client::delete_backup_keys_for_room_route)
		.ruma_route(&client::delete_backup_keys_for_session_route)
		.ruma_route(&client::delete_backup_keys_route)
		.ruma_route(&client::get_backup_keys_for_room_route)
		.ruma_route(&client::get_backup_keys_for_session_route)
		.ruma_route(&client::get_backup_keys_route)
		.ruma_route(&client::set_read_marker_route)
		.ruma_route(&client::create_receipt_route)
		.ruma_route(&client::create_typing_event_route)
		.ruma_route(&client::create_room_route)
		.ruma_route(&client::redact_event_route)
		.ruma_route(&client::report_event_route)
		.ruma_route(&client::report_room_route)
		.ruma_route(&client::create_alias_route)
		.ruma_route(&client::delete_alias_route)
		.ruma_route(&client::get_alias_route)
		.ruma_route(&client::join_room_by_id_route)
		.ruma_route(&client::join_room_by_id_or_alias_route)
		.ruma_route(&client::joined_members_route)
		.ruma_route(&client::knock_room_route)
		.ruma_route(&client::leave_room_route)
		.ruma_route(&client::forget_room_route)
		.ruma_route(&client::joined_rooms_route)
		.ruma_route(&client::kick_user_route)
		.ruma_route(&client::ban_user_route)
		.ruma_route(&client::unban_user_route)
		.ruma_route(&client::invite_user_route)
		.ruma_route(&client::set_room_visibility_route)
		.ruma_route(&client::get_room_visibility_route)
		.ruma_route(&client::get_public_rooms_route)
		.ruma_route(&client::get_public_rooms_filtered_route)
		.ruma_route(&client::search_users_route)
		.ruma_route(&client::get_member_events_route)
		.ruma_route(&client::get_protocols_route)
		.ruma_route(&client::send_message_event_route)
		.ruma_route(&client::send_state_event_for_key_route)
		.ruma_route(&client::get_state_events_route)
		.ruma_route(&client::get_state_events_for_key_route)
		// Ruma doesn't have support for multiple paths for a single endpoint yet, and these routes
		// share one Ruma request / response type pair with {get,send}_state_event_for_key_route
		.route(
			"/_matrix/client/r0/rooms/{room_id}/state/{event_type}",
			get(client::get_state_events_for_empty_key_route)
				.put(client::send_state_event_for_empty_key_route),
		)
		.route(
			"/_matrix/client/v3/rooms/{room_id}/state/{event_type}",
			get(client::get_state_events_for_empty_key_route)
				.put(client::send_state_event_for_empty_key_route),
		)
		// These two endpoints allow trailing slashes
		.route(
			"/_matrix/client/r0/rooms/{room_id}/state/{event_type}/",
			get(client::get_state_events_for_empty_key_route)
				.put(client::send_state_event_for_empty_key_route),
		)
		.route(
			"/_matrix/client/v3/rooms/{room_id}/state/{event_type}/",
			get(client::get_state_events_for_empty_key_route)
				.put(client::send_state_event_for_empty_key_route),
		)
		.ruma_route(&client::events_route)
		.ruma_route(&client::sync_events_route)
		.ruma_route(&client::sync_events_v5_route)
		.ruma_route(&client::get_context_route)
		.ruma_route(&client::get_message_events_route)
		.ruma_route(&client::search_events_route)
		.ruma_route(&client::turn_server_route)
		.ruma_route(&client::send_event_to_device_route)
		.ruma_route(&client::create_content_route)
		.ruma_route(&client::get_content_thumbnail_route)
		.ruma_route(&client::get_content_route)
		.ruma_route(&client::get_content_as_filename_route)
		.ruma_route(&client::get_media_preview_route)
		.ruma_route(&client::get_media_config_route)
		.ruma_route(&client::get_devices_route)
		.ruma_route(&client::get_device_route)
		.ruma_route(&client::update_device_route)
		.ruma_route(&client::delete_device_route)
		.ruma_route(&client::delete_devices_route)
		.ruma_route(&client::put_dehydrated_device_route)
		.ruma_route(&client::delete_dehydrated_device_route)
		.ruma_route(&client::get_dehydrated_device_route)
		.ruma_route(&client::get_dehydrated_events_route)
		.ruma_route(&client::get_tags_route)
		.ruma_route(&client::update_tag_route)
		.ruma_route(&client::delete_tag_route)
		.ruma_route(&client::upload_signing_keys_route)
		.ruma_route(&client::upload_signatures_route)
		.ruma_route(&client::get_key_changes_route)
		.ruma_route(&client::get_pushers_route)
		.ruma_route(&client::set_pushers_route)
		.ruma_route(&client::upgrade_room_route)
		.ruma_route(&client::get_threads_route)
		.ruma_route(&client::get_relating_events_with_rel_type_and_event_type_route)
		.ruma_route(&client::get_relating_events_with_rel_type_route)
		.ruma_route(&client::get_relating_events_route)
		.ruma_route(&client::get_hierarchy_route)
		.ruma_route(&client::get_mutual_rooms_route)
		.ruma_route(&client::get_room_summary)
		.route(
			"/_matrix/client/unstable/im.nheko.summary/rooms/{room_id_or_alias}/summary",
			get(client::get_room_summary_legacy)
		)
		.ruma_route(&client::well_known_support)
		.ruma_route(&client::well_known_client)
		.route("/_tuwunel/server_version", get(client::tuwunel_server_version))
		.ruma_route(&client::room_initial_sync_route)
		.route("/client/server.json", get(client::syncv3_client_server_json));

	// SS endpoints not related to federation
	router = router
		.ruma_route(&server::well_known_server)
		.ruma_route(&server::get_openid_userinfo_route);

	if config.allow_federation {
		router = router
			.ruma_route(&server::get_server_version_route)
			.route("/_matrix/key/v2/server", get(server::get_server_keys_route))
			.route(
				"/_matrix/key/v2/server/{key_id}",
				get(server::get_server_keys_deprecated_route),
			)
			.ruma_route(&server::get_public_rooms_route)
			.ruma_route(&server::get_public_rooms_filtered_route)
			.ruma_route(&server::send_transaction_message_route)
			.ruma_route(&server::get_event_route)
			.ruma_route(&server::get_backfill_route)
			.ruma_route(&server::get_missing_events_route)
			.ruma_route(&server::get_event_authorization_route)
			.ruma_route(&server::get_room_state_route)
			.ruma_route(&server::get_room_state_ids_route)
			.ruma_route(&server::create_leave_event_template_route)
			.ruma_route(&server::create_knock_event_template_route)
			.ruma_route(&server::create_leave_event_v1_route)
			.ruma_route(&server::create_leave_event_v2_route)
			.ruma_route(&server::create_knock_event_v1_route)
			.ruma_route(&server::create_join_event_template_route)
			.ruma_route(&server::create_join_event_v1_route)
			.ruma_route(&server::create_join_event_v2_route)
			.ruma_route(&server::create_invite_route)
			.ruma_route(&server::get_devices_route)
			.ruma_route(&server::get_room_information_route)
			.ruma_route(&server::get_profile_information_route)
			.ruma_route(&server::get_keys_route)
			.ruma_route(&server::claim_keys_route)
			.ruma_route(&server::get_hierarchy_route)
			.ruma_route(&server::get_content_route)
			.ruma_route(&server::get_content_thumbnail_route)
			.route("/_tuwunel/local_user_count", get(client::tuwunel_local_user_count));
	} else {
		router = router
			.route("/_matrix/federation/{*path}", any(federation_disabled))
			.route("/_matrix/key/{*path}", any(federation_disabled))
			.route("/_tuwunel/local_user_count", any(federation_disabled));
	}

	if config.allow_legacy_media {
		router = router
			.ruma_route(&client::get_media_config_legacy_route)
			.ruma_route(&client::get_media_preview_legacy_route)
			.ruma_route(&client::get_content_legacy_route)
			.ruma_route(&client::get_content_as_filename_legacy_route)
			.ruma_route(&client::get_content_thumbnail_legacy_route)
			.route("/_matrix/media/v1/config", get(client::get_media_config_legacy_legacy_route))
			.route("/_matrix/media/v1/upload", post(client::create_content_legacy_route))
			.route(
				"/_matrix/media/v1/preview_url",
				get(client::get_media_preview_legacy_legacy_route),
			)
			.route(
				"/_matrix/media/v1/download/{server_name}/{media_id}",
				get(client::get_content_legacy_legacy_route),
			)
			.route(
				"/_matrix/media/v1/download/{server_name}/{media_id}/{file_name}",
				get(client::get_content_as_filename_legacy_legacy_route),
			)
			.route(
				"/_matrix/media/v1/thumbnail/{server_name}/{media_id}",
				get(client::get_content_thumbnail_legacy_legacy_route),
			);
	} else {
		router = router
			.route("/_matrix/media/v1/{*path}", any(legacy_media_disabled))
			.route("/_matrix/media/v3/config", any(legacy_media_disabled))
			.route("/_matrix/media/v3/download/{*path}", any(legacy_media_disabled))
			.route("/_matrix/media/v3/thumbnail/{*path}", any(legacy_media_disabled))
			.route("/_matrix/media/v3/preview_url", any(redirect_legacy_preview))
			.route("/_matrix/media/r0/config", any(legacy_media_disabled))
			.route("/_matrix/media/r0/download/{*path}", any(legacy_media_disabled))
			.route("/_matrix/media/r0/thumbnail/{*path}", any(legacy_media_disabled))
			.route("/_matrix/media/r0/preview_url", any(redirect_legacy_preview));
	}

	router
}

async fn redirect_legacy_preview(uri: Uri) -> impl IntoResponse {
	let path = "/_matrix/client/v1/media/preview_url";
	let query = uri.query().unwrap_or_default();

	let path_and_query = format!("{path}?{query}");
	let path_and_query = uri::PathAndQuery::from_str(&path_and_query)
		.expect("Failed to build PathAndQuery for media preview redirect URI");

	let uri = uri::Builder::new()
		.path_and_query(path_and_query)
		.build()
		.expect("Failed to build URI for redirect")
		.to_string();

	Redirect::temporary(&uri)
}

async fn legacy_media_disabled() -> impl IntoResponse {
	err!(Request(Forbidden("Unauthenticated media is disabled.")))
}

async fn federation_disabled() -> impl IntoResponse {
	err!(Request(Forbidden("Federation is disabled.")))
}
