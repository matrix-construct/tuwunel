use axum::Router;
use tuwunel_api::router::RouterExt;

use super::State;
use crate as mas;

/// Builds the Matrix Authentication Service endpoint routes.
///
/// The returned router registers the account and device integration handlers.
pub fn build() -> Router<State> {
	let router = Router::new();

	register_mas_routes(router)
}

fn register_mas_routes(router: Router<State>) -> Router<State> {
	router
		.ruma_route(&mas::query_user_route)
		.ruma_route(&mas::provision_user_route)
		.ruma_route(&mas::is_localpart_available_route)
		.ruma_route(&mas::delete_user_route)
		.ruma_route(&mas::reactivate_user_route)
		.ruma_route(&mas::set_displayname_route)
		.ruma_route(&mas::unset_displayname_route)
		.ruma_route(&mas::allow_cross_signing_reset_route)
		.ruma_route(&mas::upsert_device_route)
		.ruma_route(&mas::delete_device_route)
		.ruma_route(&mas::update_device_display_name_route)
		.ruma_route(&mas::sync_devices_route)
}
