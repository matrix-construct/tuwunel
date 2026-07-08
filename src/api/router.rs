mod args;
mod auth;
mod client_ip;
mod handler;
mod request;
mod response;
pub mod state;

use axum::{
	Router,
	response::IntoResponse,
	routing::{any, get, post},
};
pub use client_ip::{ConfiguredIpSource, TrustedPeerSubnets};
use tuwunel_core::{Server, err};

use self::handler::RouterExt;
pub(super) use self::{
	args::Args as Ruma, auth::auth_uiaa, client_ip::ClientIp, response::RumaResponse,
	state::State,
};
use crate::{
	client::{self, mas_active},
	oidc::{self, native_get_route, native_submit_route},
	server,
};

pub fn build(router: Router<State>, server: &Server) -> Router<State> {
	let config = &server.config;
	let mas_active = mas_active(config);
	let router = register_client_auth_routes(router);
	let router = register_mas_routes(router);
	let router = register_client_profile_and_data_routes(router);
	let router = register_client_keys_and_backup_routes(router);
	let router = register_client_room_routes(router);
	let router = register_client_state_and_sync_routes(router);
	let router = register_client_media_and_device_routes(router);
	let router = register_client_misc_routes(router);
	let router = register_synapse_admin_users_routes(router, mas_active);
	let router = register_synapse_admin_devices_routes(router, mas_active);
	let router = register_synapse_admin_rooms_routes(router);
	let router = register_synapse_admin_media_routes(router);
	let router = register_synapse_admin_federation_routes(router);
	let router = register_synapse_admin_misc_routes(router);
	let router = register_oidc_routes(router);
	let router = register_server_misc_routes(router);
	let router = register_federation_routes(router, config.allow_federation);

	register_legacy_media_routes(router, config.allow_legacy_media)
}

fn register_client_auth_routes(router: Router<State>) -> Router<State> {
	router
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
		.ruma_route(&client::sso_fallback_route)
		.ruma_route(&client::whoami_route)
		.ruma_route(&client::logout_route)
		.ruma_route(&client::logout_all_route)
		.ruma_route(&client::change_password_route)
		.ruma_route(&client::deactivate_route)
		.ruma_route(&client::third_party_route)
		.ruma_route(&client::add_3pid_route)
		.ruma_route(&client::delete_3pid_route)
		.ruma_route(&client::request_3pid_management_token_via_email_route)
		.ruma_route(&client::request_3pid_management_token_via_msisdn_route)
		.ruma_route(&client::request_registration_token_via_email_route)
		.ruma_route(&client::request_password_change_token_via_email_route)
		.ruma_route(&client::check_registration_token_validity)
		.ruma_route(&client::create_openid_token_route)
		.ruma_route(&client::is_user_suspended_route)
		.ruma_route(&client::suspend_user_route)
		.ruma_route(&client::is_user_locked_route)
		.ruma_route(&client::lock_user_route)
}

fn register_mas_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::mas::query_user_route)
		.ruma_route(&client::mas::provision_user_route)
		.ruma_route(&client::mas::is_localpart_available_route)
		.ruma_route(&client::mas::delete_user_route)
		.ruma_route(&client::mas::reactivate_user_route)
		.ruma_route(&client::mas::set_displayname_route)
		.ruma_route(&client::mas::unset_displayname_route)
		.ruma_route(&client::mas::allow_cross_signing_reset_route)
		.ruma_route(&client::mas::upsert_device_route)
		.ruma_route(&client::mas::delete_device_route)
		.ruma_route(&client::mas::update_device_display_name_route)
		.ruma_route(&client::mas::sync_devices_route)
}

fn register_synapse_admin_users_routes(router: Router<State>, mas_active: bool) -> Router<State> {
	let router = router
		.ruma_route(&client::users::admin_list_users_v2_route)
		.ruma_route(&client::users::admin_list_users_v3_route)
		.ruma_route(&client::users::admin_get_details_route)
		.ruma_route(&client::users::admin_create_or_modify_route)
		.ruma_route(&client::users::admin_deactivate_account_route)
		.ruma_route(&client::users::admin_list_joined_rooms_route)
		.ruma_route(&client::users::admin_memberships_route)
		.ruma_route(&client::users::admin_pushers_route)
		.ruma_route(&client::users::admin_account_data_route)
		.ruma_route(&client::users::admin_suspend_route)
		.ruma_route(&client::users::admin_username_available_route)
		.ruma_route(&client::users::admin_lookup_threepid_route)
		.ruma_route(&client::users::admin_allow_cross_signing_replacement_route)
		// whois is served at the /_synapse admin path and the client-server admin
		// aliases where Synapse also mounts it.
		.route("/_synapse/admin/v1/whois/{user_id}", get(client::users::admin_whois_route))
		.route("/_matrix/client/v3/admin/whois/{user_id}", get(client::users::admin_whois_route))
		.route("/_matrix/client/r0/admin/whois/{user_id}", get(client::users::admin_whois_route))
		.route(
			"/_matrix/client/unstable/admin/whois/{user_id}",
			get(client::users::admin_whois_route),
		);

	// The Synapse user-provisioning routes MAS owns (register pair, password
	// reset, admin flag) are de-registered under MAS delegation, mirroring
	// Synapse.
	if mas_active {
		router
	} else {
		router
			.ruma_route(&client::admin_register_nonce_route)
			.ruma_route(&client::admin_register_route)
			.ruma_route(&client::users::admin_reset_password_route)
			.ruma_route(&client::users::admin_is_user_admin_route)
	}
}

fn register_synapse_admin_devices_routes(
	router: Router<State>,
	mas_active: bool,
) -> Router<State> {
	let router = router
		.ruma_route(&client::devices::admin_list_devices_route)
		.ruma_route(&client::devices::admin_create_device_route)
		.ruma_route(&client::devices::admin_get_device_route)
		.ruma_route(&client::devices::admin_update_device_route)
		.ruma_route(&client::devices::admin_delete_device_route)
		.ruma_route(&client::devices::admin_delete_devices_route);

	// Registration-token administration is de-registered under MAS, mirroring
	// Synapse.
	if mas_active {
		router
	} else {
		router
			.ruma_route(&client::tokens::admin_list_tokens_route)
			.ruma_route(&client::tokens::admin_create_token_route)
			.ruma_route(&client::tokens::admin_get_token_route)
			.ruma_route(&client::tokens::admin_update_token_route)
			.ruma_route(&client::tokens::admin_delete_token_route)
	}
}

fn register_synapse_admin_rooms_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::rooms::admin_list_rooms_route)
		.ruma_route(&client::rooms::admin_room_details_route)
		.ruma_route(&client::rooms::admin_room_members_route)
		.ruma_route(&client::rooms::admin_join_room_route)
		.ruma_route(&client::rooms::admin_get_room_block_route)
		.ruma_route(&client::rooms::admin_set_room_block_route)
		.ruma_route(&client::rooms::admin_make_room_admin_route)
}

fn register_synapse_admin_media_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::admin::media::admin_delete_media_route)
		.ruma_route(&client::admin::media::admin_list_user_media_route)
		.ruma_route(&client::admin::media::admin_delete_user_media_route)
		.route(
			"/_synapse/admin/v1/media/delete",
			post(client::admin::media::admin_delete_media_by_date_size_route),
		)
		// Synapse's deprecated per-server alias for the same date/size purge.
		.route(
			"/_synapse/admin/v1/media/{server_name}/delete",
			post(client::admin::media::admin_delete_media_by_date_size_route),
		)
}

fn register_synapse_admin_federation_routes(router: Router<State>) -> Router<State> { router }

fn register_synapse_admin_misc_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::misc::admin_server_version_route)
		.ruma_route(&client::misc::admin_fetch_event_route)
}

fn register_client_profile_and_data_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::get_profile_field_route)
		.ruma_route(&client::set_profile_field_route)
		.ruma_route(&client::delete_profile_field_route)
		.ruma_route(&client::get_profile_route)
		.ruma_route(&client::set_presence_route)
		.ruma_route(&client::get_presence_route)
		.ruma_route(&client::get_filter_route)
		.ruma_route(&client::create_filter_route)
		.ruma_route(&client::set_global_account_data_route)
		.ruma_route(&client::set_room_account_data_route)
		.ruma_route(&client::get_global_account_data_route)
		.ruma_route(&client::get_room_account_data_route)
		.ruma_route(&client::delete_global_account_data_route)
		.ruma_route(&client::delete_room_account_data_route)
		.ruma_route(&client::get_tags_route)
		.ruma_route(&client::update_tag_route)
		.ruma_route(&client::delete_tag_route)
		.ruma_route(&client::get_pushrules_all_route)
		.ruma_route(&client::get_pushrules_global_route)
		.ruma_route(&client::set_pushrule_route)
		.ruma_route(&client::get_pushrule_route)
		.ruma_route(&client::set_pushrule_enabled_route)
		.ruma_route(&client::get_pushrule_enabled_route)
		.ruma_route(&client::get_pushrule_actions_route)
		.ruma_route(&client::set_pushrule_actions_route)
		.ruma_route(&client::delete_pushrule_route)
		.ruma_route(&client::get_pushers_route)
		.ruma_route(&client::set_pushers_route)
		.ruma_route(&client::get_notifications_route)
		.ruma_route(&client::get_capabilities_route)
}

fn register_client_keys_and_backup_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::upload_keys_route)
		.ruma_route(&client::get_keys_route)
		.ruma_route(&client::claim_keys_route)
		.ruma_route(&client::upload_signing_keys_route)
		.ruma_route(&client::upload_signatures_route)
		.ruma_route(&client::get_key_changes_route)
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
}

fn register_client_room_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::appservice_ping)
		.ruma_route(&client::set_read_marker_route)
		.ruma_route(&client::create_receipt_route)
		.ruma_route(&client::create_typing_event_route)
		.ruma_route(&client::create_room_route)
		.ruma_route(&client::redact_event_route)
		.ruma_route(&client::report_event_route)
		.ruma_route(&client::report_room_route)
		.ruma_route(&client::report_user_route)
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
		.ruma_route(&client::upgrade_room_route)
		.ruma_route(&client::get_mutual_rooms_route)
		.ruma_route(&client::get_room_summary)
		.route(
			"/_matrix/client/unstable/im.nheko.summary/rooms/{room_id_or_alias}/summary",
			get(client::get_room_summary_legacy),
		)
		.ruma_route(&client::room_initial_sync_route)
		.ruma_route(&client::get_room_event_route)
		.ruma_route(&client::get_room_aliases_route)
}

fn register_client_state_and_sync_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::send_message_event_route)
		.ruma_route(&client::send_state_event_for_key_route)
		.ruma_route(&client::get_state_events_route)
		.ruma_route(&client::get_state_events_for_key_route)
		// Ruma doesn't have support for multiple paths for a single endpoint yet, and these
		// routes share one Ruma request / response type pair with
		// {get,send}_state_event_for_key_route
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
		.ruma_route(&client::get_event_by_timestamp_route)
		.ruma_route(&client::get_message_events_route)
		.ruma_route(&client::search_events_route)
		.ruma_route(&client::get_threads_route)
		.ruma_route(&client::get_relating_events_with_rel_type_and_event_type_route)
		.ruma_route(&client::get_relating_events_with_rel_type_route)
		.ruma_route(&client::get_relating_events_route)
		.ruma_route(&client::get_hierarchy_route)
}

fn register_client_media_and_device_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::create_content_route)
		.ruma_route(&client::create_mxc_uri_route)
		.ruma_route(&client::create_content_async_route)
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
		.ruma_route(&client::send_event_to_device_route)
}

fn register_client_misc_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::turn_server_route)
		.ruma_route(&client::get_transports_route)
		.ruma_route(&client::well_known_support)
		.ruma_route(&client::well_known_client)
		.ruma_route(&client::tuwunel_remote_version)
		.route("/_tuwunel/server_version", get(client::tuwunel_server_version))
		.route(
			"/_tuwunel/3pid/email/validate",
			get(client::get_email_validate_route).post(client::post_email_validate_route),
		)
}

fn register_oidc_routes(router: Router<State>) -> Router<State> {
	// OIDC server endpoints (next-gen auth, MSC2965/2964/2966/2967)
	router
		.route("/_tuwunel/oidc/registration", post(oidc::registration_route))
		.route("/_tuwunel/oidc/authorize", get(oidc::authorize_route))
		.route("/_tuwunel/oidc/_complete", get(oidc::complete_route))
		.route("/_tuwunel/oidc/native", get(native_get_route).post(native_submit_route))
		.route("/_tuwunel/oidc/token", post(oidc::token_route))
		.route("/_tuwunel/oidc/device_authorization", post(oidc::device_authorization_route))
		.route("/_tuwunel/oidc/device", get(oidc::get_device_route))
		.route(
			"/_tuwunel/oidc/device_callback",
			get(oidc::get_device_callback_route).post(oidc::post_device_callback_route),
		)
		.route("/_tuwunel/oidc/revoke", post(oidc::revoke_route))
		.route("/_tuwunel/oidc/jwks", get(oidc::jwks_route))
		.route("/_tuwunel/oidc/userinfo", get(oidc::userinfo_route).post(oidc::userinfo_route))
		.route("/_tuwunel/oidc/account.js", get(oidc::account_js_route))
		.route("/_tuwunel/oidc/account.css", get(oidc::account_css_route))
		.route(
			"/_tuwunel/oidc/account_callback",
			get(oidc::get_account_callback_route).post(oidc::post_account_callback_route),
		)
		.route("/_tuwunel/oidc/account", get(oidc::get_account_route))
		.route("/_matrix/client/v1/auth_issuer", get(oidc::auth_issuer_route))
		.route("/_matrix/client/v1/auth_metadata", get(oidc::openid_configuration_route))
		.route(
			"/_matrix/client/unstable/org.matrix.msc2965/auth_issuer",
			get(oidc::auth_issuer_route),
		)
		.route(
			"/_matrix/client/unstable/org.matrix.msc2965/auth_metadata",
			get(oidc::openid_configuration_route),
		)
		.route("/.well-known/openid-configuration", get(oidc::openid_configuration_route))
}

fn register_server_misc_routes(router: Router<State>) -> Router<State> {
	// SS endpoints not related to federation
	router
		.ruma_route(&server::well_known_server)
		.ruma_route(&server::get_openid_userinfo_route)
}

fn register_federation_routes(router: Router<State>, allow_federation: bool) -> Router<State> {
	if allow_federation {
		router
			.ruma_route(&server::get_server_version_route)
			.route("/_matrix/key/v2/server", get(server::get_server_keys_route))
			.ruma_route(&server::get_public_rooms_route)
			.ruma_route(&server::get_public_rooms_filtered_route)
			.ruma_route(&server::send_transaction_message_route)
			.ruma_route(&server::get_event_route)
			.ruma_route(&server::get_event_by_timestamp_route)
			.ruma_route(&server::get_backfill_route)
			.ruma_route(&server::get_missing_events_route)
			.ruma_route(&server::get_event_authorization_route)
			.ruma_route(&server::get_room_state_route)
			.ruma_route(&server::get_room_state_ids_route)
			.ruma_route(&server::create_leave_event_template_route)
			.ruma_route(&server::create_knock_event_template_route)
			.ruma_route(&server::create_leave_event_v2_route)
			.ruma_route(&server::create_knock_event_v1_route)
			.ruma_route(&server::create_join_event_template_route)
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
			.route("/_matrix/federation/v1/query/edutypes", get(server::get_edu_types_route))
			.route("/_tuwunel/local_user_count", get(client::tuwunel_local_user_count))
	} else {
		router
			.route("/_matrix/federation/{*path}", any(federation_disabled))
			.route("/_matrix/key/{*path}", any(federation_disabled))
			.route("/_tuwunel/local_user_count", any(federation_disabled))
	}
}

fn register_legacy_media_routes(
	router: Router<State>,
	allow_legacy_media: bool,
) -> Router<State> {
	if allow_legacy_media {
		router
			.ruma_route(&client::get_media_config_legacy_route)
			.ruma_route(&client::get_media_preview_legacy_route)
			.route(
				"/_matrix/media/r0/download/{server_name}/{media_id}",
				get(client::get_content_legacy_route),
			)
			.route(
				"/_matrix/media/v3/download/{server_name}/{media_id}",
				get(client::get_content_legacy_route),
			)
			.route(
				"/_matrix/media/r0/download/{server_name}/{media_id}/{filename}",
				get(client::get_content_as_filename_legacy_route),
			)
			.route(
				"/_matrix/media/v3/download/{server_name}/{media_id}/{filename}",
				get(client::get_content_as_filename_legacy_route),
			)
			.route(
				"/_matrix/media/r0/thumbnail/{server_name}/{media_id}",
				get(client::get_content_thumbnail_legacy_route),
			)
			.route(
				"/_matrix/media/v3/thumbnail/{server_name}/{media_id}",
				get(client::get_content_thumbnail_legacy_route),
			)
	} else {
		router
			.route("/_matrix/media/v3/config", any(legacy_media_disabled))
			.route("/_matrix/media/v3/download/{*path}", any(legacy_media_disabled))
			.route("/_matrix/media/v3/thumbnail/{*path}", any(legacy_media_disabled))
			.route("/_matrix/media/v3/preview_url", any(legacy_media_disabled))
	}
}

async fn legacy_media_disabled() -> impl IntoResponse {
	err!(Request(Forbidden("Unauthenticated media is disabled.")))
}

async fn federation_disabled() -> impl IntoResponse {
	err!(Request(Forbidden("Federation is disabled.")))
}
