use axum::extract::State;
use ruma::api::{error::ErrorKind, federation::discovery::discover_homeserver};
use tuwunel_core::{Error, Result};

use crate::Ruma;

/// # `GET /.well-known/matrix/server`
///
/// Returns the .well-known URL if it is configured, otherwise returns 404.
pub(crate) async fn well_known_server(
	State(services): State<crate::State>,
	_body: Ruma<discover_homeserver::Request>,
) -> Result<discover_homeserver::Response> {
	Ok(discover_homeserver::Response {
		server: match services.server.config.well_known.server.as_ref() {
			| Some(server_name) => server_name.to_owned(),
			| None => return Err(Error::BadRequest(ErrorKind::NotFound, "Not found.")),
		},
	})
}
