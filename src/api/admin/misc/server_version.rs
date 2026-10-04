use synapse_admin_api::version::get_server_version::v1::{self as server_version, Response};
use tuwunel_core::{Result, version::version};

use crate::Ruma;

/// # `GET /_synapse/admin/v1/server_version`
///
/// Reports the running server version. Unauthenticated, matching Synapse.
pub(crate) async fn admin_server_version_route(
	_body: Ruma<server_version::Request>,
) -> Result<Response> {
	Ok(Response::new(version().to_owned()))
}
