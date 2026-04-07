use axum::{
	extract::State,
	response::{IntoResponse, Redirect},
};
use tuwunel_core::{Result, err};

pub(crate) async fn account_route(
	State(services): State<crate::State>,
) -> Result<impl IntoResponse> {
	let default_idp_id = services
		.oauth
		.providers
		.get_default_id()
		.ok_or_else(|| err!(Config("identity_provider", "No identity provider configured")))?;

	// Redirect to the identity provider's panel where users can manage
	// their account, sessions, devices, and profile.
	let idp = services
		.oauth
		.providers
		.get(&default_idp_id)
		.await?;

	let panel_url = idp.issuer_url.as_ref().ok_or_else(|| {
		err!(Config("issuer_url", "issuer_url is required for account management redirect"))
	})?;

	Ok(Redirect::temporary(panel_url.as_str()))
}
