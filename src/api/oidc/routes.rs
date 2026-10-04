use axum::{
	Router,
	routing::{get, post},
};
use tuwunel_api::router::State;

// Aliased to keep the subsystem visible where the sibling routes stay qualified.
use crate::{
	self as oidc, complete_route as oidc_complete, post_complete_route as oidc_post_complete,
};

/// Builds the OpenID Connect endpoint routes.
///
/// The returned router registers discovery, authorization and callback handlers.
pub fn build() -> Router<State> {
	let router = Router::new();

	register_oidc_routes(router)
}

fn register_oidc_routes(router: Router<State>) -> Router<State> {
	// OIDC server endpoints (next-gen auth, MSC2965/2964/2966/2967)
	router
		.route("/_tuwunel/oidc/registration", post(oidc::registration_route))
		.route("/_tuwunel/oidc/authorize", get(oidc::authorize_route))
		.route("/_tuwunel/oidc/_complete", get(oidc_complete).post(oidc_post_complete))
		.route(
			"/_tuwunel/oidc/native",
			get(oidc::native_get_route).post(oidc::native_submit_route),
		)
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
