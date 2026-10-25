use std::collections::BTreeMap;

use axum::extract::State;
use tuwunel_core::Result;
use tuwunel_service::rooms::reconcile::api;

use crate::Ruma;

/// # `GET /_matrix/federation/v1/version`
///
/// Get version information on this server.
pub(crate) async fn get_server_version_route(
	State(services): State<crate::State>,
	_body: Ruma<api::version::Request>,
) -> Result<api::version::Response> {
	let unstable_features = services.server.config.msc4521_enabled.then(|| {
		BTreeMap::from([
			("tk.nutra.msc0501.reconciliation".to_owned(), true),
			("tk.nutra.msc4521.digest.algebraic_v1.0.0".to_owned(), true),
			("tk.nutra.msc4521.binding.event_id_v1".to_owned(), true),
			("tk.nutra.msc0501.strata".to_owned(), true),
		])
	});
	Ok(api::version::Response {
		server: Some(ruma::api::federation::discovery::get_server_version::v1::Server {
			name: Some(tuwunel_core::version::name().into()),
			version: Some(tuwunel_core::version::version().into()),
			compiler: tuwunel_core::info::rustc::version().map(Into::into),
			..Default::default()
		}),
		unstable_features,
	})
}
