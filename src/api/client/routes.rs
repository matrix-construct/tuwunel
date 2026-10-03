use axum::{
	Router,
	response::IntoResponse,
	routing::{any, get, post},
};
use const_str::{concat, join, replace};
use http::{HeaderValue, header};
use tower_http::set_header::SetResponseHeaderLayer;
use tuwunel_core::{Server, err};

use crate::{State, client, router::RouterExt};

/// Builds the client endpoint routes.
///
/// Federation and media settings select the configured route variants.
pub fn build(server: &Server) -> Router<State> {
	let config = &server.config;
	let router = Router::new();
	let router = register_client_auth_routes(router);
	let router = register_client_profile_and_data_routes(router);
	let router = register_client_keys_and_backup_routes(router);
	let router = register_client_room_routes(router);
	let router = register_client_state_and_sync_routes(router);
	let router = register_client_media_and_device_routes(
		router,
		config.media_deny_framing,
		config.media_deny_inline_styles,
	);

	let router = register_client_misc_routes(router);
	let router = register_rendezvous_routes(router);
	let router = if config.allow_federation {
		router.route("/_tuwunel/local_user_count", get(client::tuwunel_local_user_count))
	} else {
		router
	};

	register_legacy_media_routes(
		router,
		config.allow_legacy_media,
		config.media_deny_framing,
		config.media_deny_inline_styles,
	)
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
		.route("/_tuwunel/sso/complete.js", get(client::sso_complete_js_route))
		.route("/_tuwunel/sso/sso.css", get(client::sso_css_route))
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
		.ruma_route(&client::get_protocol_route)
		.ruma_route(&client::get_user_for_protocol_route)
		.ruma_route(&client::get_location_for_protocol_route)
		.ruma_route(&client::get_user_for_user_id_route)
		.ruma_route(&client::get_location_for_room_alias_route)
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

fn register_client_media_and_device_routes(
	router: Router<State>,
	media_deny_framing: bool,
	media_deny_inline_styles: bool,
) -> Router<State> {
	let media_content_router = Router::new()
		.ruma_route(&client::get_content_thumbnail_route)
		.ruma_route(&client::get_content_route)
		.ruma_route(&client::get_content_as_filename_route);

	let media_content_router =
		media_content_headers(media_content_router, media_deny_framing, media_deny_inline_styles);

	router
		.ruma_route(&client::create_content_route)
		.ruma_route(&client::create_mxc_uri_route)
		.ruma_route(&client::create_content_async_route)
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
		.merge(media_content_router)
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

fn register_rendezvous_routes(router: Router<State>) -> Router<State> {
	let router = router
		.ruma_route(&client::discover_msc4388_route)
		.ruma_route(&client::create_msc4388_route)
		.ruma_route(&client::get_msc4388_route)
		.ruma_route(&client::put_msc4388_route)
		.ruma_route(&client::delete_msc4388_route);

	let session_routes = get(client::get_rendezvous_route)
		.put(client::put_rendezvous_route)
		.delete(client::delete_rendezvous_route);

	router
		.route(
			"/_matrix/client/unstable/org.matrix.msc4108/rendezvous",
			post(client::create_rendezvous_route),
		)
		.route("/_matrix/client/unstable/org.matrix.msc4108/rendezvous/{id}", session_routes)
}

fn register_legacy_media_routes(
	router: Router<State>,
	allow_legacy_media: bool,
	media_deny_framing: bool,
	media_deny_inline_styles: bool,
) -> Router<State> {
	if allow_legacy_media {
		let media_content_router = Router::new()
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
			);

		let media_content_router = media_content_headers(
			media_content_router,
			media_deny_framing,
			media_deny_inline_styles,
		);

		router
			.ruma_route(&client::get_media_config_legacy_route)
			.ruma_route(&client::get_media_preview_legacy_route)
			.merge(media_content_router)
	} else {
		router
			.route("/_matrix/media/v3/config", any(legacy_media_disabled))
			.route("/_matrix/media/v3/download/{*path}", any(legacy_media_disabled))
			.route("/_matrix/media/v3/thumbnail/{*path}", any(legacy_media_disabled))
			.route("/_matrix/media/v3/preview_url", any(legacy_media_disabled))
	}
}

fn media_content_headers(
	router: Router<State>,
	deny_framing: bool,
	deny_inline_styles: bool,
) -> Router<State> {
	// The MSC4149 media policy, stricter than the spec's recommendation.
	const MEDIA_CSP: &[&str] = &[
		"sandbox",
		"default-src 'none'",
		"script-src 'none'",
		"font-src 'none'",
		"frame-ancestors 'none'",
		"form-action 'none'",
		"base-uri 'none'",
	];

	const POLICY: &str = join!(MEDIA_CSP, ";");
	const FRAMING_POLICY: &str = replace!(POLICY, "frame-ancestors 'none';", "");
	const STYLED_POLICY: &str = concat!(POLICY, ";style-src 'unsafe-inline'");
	const FRAMING_STYLED_POLICY: &str = concat!(FRAMING_POLICY, ";style-src 'unsafe-inline'");

	let policy = match (deny_framing, deny_inline_styles) {
		| (false, false) => FRAMING_STYLED_POLICY,
		| (false, true) => FRAMING_POLICY,
		| (true, false) => STYLED_POLICY,
		| (true, true) => POLICY,
	};

	router.route_layer(SetResponseHeaderLayer::overriding(
		header::CONTENT_SECURITY_POLICY,
		HeaderValue::from_static(policy),
	))
}

async fn legacy_media_disabled() -> impl IntoResponse {
	err!(Request(Forbidden("Unauthenticated media is disabled.")))
}
