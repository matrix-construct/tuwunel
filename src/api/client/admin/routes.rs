use axum::{
	Router,
	routing::{get, post},
};
use tuwunel_core::Server;

use crate::{State, client, client::mas_active, router::RouterExt};

/// Builds the administrator endpoint routes.
///
/// The authentication mode selects the available user and device routes.
pub fn build(server: &Server) -> Router<State> {
	let mas_active = mas_active(&server.config);
	let router = Router::new();
	let router = register_synapse_admin_users_routes(router, mas_active);
	let router = register_synapse_admin_devices_routes(router, mas_active);
	let router = register_synapse_admin_rooms_routes(router);
	let router = register_synapse_admin_media_routes(router);
	let router = register_synapse_admin_federation_routes(router);
	let router = register_synapse_admin_misc_routes(router);

	router
		.ruma_route(&client::is_user_suspended_route)
		.ruma_route(&client::suspend_user_route)
		.ruma_route(&client::is_user_locked_route)
		.ruma_route(&client::lock_user_route)
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
		.ruma_route(&client::users::admin_redact_user_route)
		.ruma_route(&client::users::admin_redact_status_route)
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
			.ruma_route(&client::users::admin_login_as_route)
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
		.ruma_route(&client::rooms::admin_delete_room_v1_route)
		.ruma_route(&client::rooms::admin_delete_room_v2_route)
		.ruma_route(&client::rooms::admin_delete_status_by_id_route)
		.ruma_route(&client::rooms::admin_delete_status_by_room_route)
		.ruma_route(&client::rooms::admin_get_forward_extremities_route)
		.ruma_route(&client::rooms::admin_delete_forward_extremities_route)
		.ruma_route(&client::rooms::admin_purge_history_route)
		.ruma_route(&client::rooms::admin_purge_history_by_event_route)
		.ruma_route(&client::rooms::admin_purge_history_status_route)
		.ruma_route(&client::rooms::admin_room_state_route)
		.ruma_route(&client::rooms::admin_room_messages_route)
		.ruma_route(&client::rooms::admin_room_context_route)
		.ruma_route(&client::rooms::admin_room_timestamp_to_event_route)
		.ruma_route(&client::rooms::admin_room_hierarchy_route)
}

fn register_synapse_admin_media_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::admin::media::admin_query_media_route)
		.ruma_route(&client::admin::media::admin_delete_media_route)
		.ruma_route(&client::admin::media::admin_list_user_media_route)
		.ruma_route(&client::admin::media::admin_list_room_media_route)
		.ruma_route(&client::admin::media::admin_delete_user_media_route)
		.ruma_route(&client::admin::media::admin_purge_media_cache_route)
		.ruma_route(&client::admin::media::admin_user_media_statistics_route)
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

fn register_synapse_admin_federation_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::admin::federation::admin_list_destinations_route)
		.ruma_route(&client::admin::federation::admin_destination_details_route)
		.ruma_route(&client::admin::federation::admin_destination_rooms_route)
		.ruma_route(&client::admin::federation::admin_reset_connection_route)
}

fn register_synapse_admin_misc_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&client::misc::admin_server_version_route)
		.ruma_route(&client::misc::admin_fetch_event_route)
		.ruma_route(&client::misc::admin_scheduled_tasks_route)
		.ruma_route(&client::misc::admin_send_server_notice_route)
		.ruma_route(&client::misc::admin_send_server_notice_txn_route)
}
