use axum::{
	Router,
	response::IntoResponse,
	routing::{any, get},
};
use tuwunel_core::{Server, err};

use crate::{State, router::RouterExt, server};

/// Builds the server endpoint routes.
///
/// Federation settings select the enabled handlers or disabled responses.
pub fn build(server: &Server) -> Router<State> {
	let router = Router::new();
	let router = register_server_misc_routes(router);

	register_federation_routes(router, server.config.allow_federation)
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
	} else {
		router
			.route("/_matrix/federation/{*path}", any(federation_disabled))
			.route("/_matrix/key/{*path}", any(federation_disabled))
			.route("/_tuwunel/local_user_count", any(federation_disabled))
	}
}

async fn federation_disabled() -> impl IntoResponse {
	err!(Request(Forbidden("Federation is disabled.")))
}
