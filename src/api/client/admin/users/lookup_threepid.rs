use axum::extract::State;
use synapse_admin_api::users::lookup_threepid::v1 as lookup_threepid;
use tuwunel_core::{Err, Result, err};
use tuwunel_service::threepid::canonicalize_email;

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/threepid/{medium}/users/{address}`
///
/// Only the `email` medium is backed by a binding store; any other medium has
/// no associated user.
pub(crate) async fn admin_lookup_threepid_route(
	State(services): State<crate::State>,
	body: Ruma<lookup_threepid::Request>,
) -> Result<lookup_threepid::Response> {
	require_admin(&services, body.sender_user()).await?;

	if !body.medium.eq_ignore_ascii_case("email") {
		return Err!(Request(NotFound("User not found")));
	}

	let email = canonicalize_email(&body.address)
		.map_err(|_| err!(Request(NotFound("User not found"))))?;

	let user_id = services
		.threepid
		.user_id_for_email(&email)
		.await?
		.ok_or_else(|| err!(Request(NotFound("User not found"))))?;

	Ok(lookup_threepid::Response::new(user_id))
}
