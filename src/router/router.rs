use std::sync::Arc;

use axum::{Router, response::IntoResponse, routing::get};
use http::{StatusCode, Uri};
use ruma::api::error::ErrorKind;
use tuwunel_api::{
	client::{admin::routes::build as admin_routes, routes::build as client_routes},
	router::{state, state::Guard},
	server::routes::build as server_routes,
};
use tuwunel_api_mas::routes::build as mas_routes;
use tuwunel_api_oidc::routes::build as oidc_routes;
use tuwunel_core::Error;
use tuwunel_service::Services;

pub(crate) fn build(services: &Arc<Services>) -> (Router, Guard) {
	let router = Router::<state::State>::new();
	let (state, guard) = state::create(services.clone());
	let router = router
		.merge(client_routes(&services.server))
		.merge(admin_routes(&services.server))
		.merge(mas_routes())
		.merge(oidc_routes())
		.merge(server_routes(&services.server))
		.route("/", get(it_works))
		.fallback(not_found)
		.method_not_allowed_fallback(method_not_allowed)
		.with_state(state);

	(router, guard)
}

async fn not_found(_uri: Uri) -> impl IntoResponse {
	Error::Request(ErrorKind::Unrecognized, "Not Found".into(), StatusCode::NOT_FOUND)
}

// Bare 405; request::handle_result formats the M_UNRECOGNIZED body.
async fn method_not_allowed() -> StatusCode { StatusCode::METHOD_NOT_ALLOWED }

async fn it_works() -> &'static str { "hewwo from tuwunel woof!" }
