use axum::Router;

use crate::{State, client, router::RouterExt};

/// Builds the Matrix Authentication Service endpoint routes.
///
/// The returned router registers the account and device integration handlers.
pub fn build() -> Router<State> {
	let router = Router::new();

	register_mas_routes(router)
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
