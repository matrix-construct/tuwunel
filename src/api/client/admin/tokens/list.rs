use axum::extract::State;
use futures::StreamExt;
use synapse_admin_api::registration_tokens::list::v1 as list;
use tuwunel_core::Result;

use super::valid_token_response;
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/registration_tokens`
///
/// The store retains only valid tokens (expired or exhausted ones are cleaned
/// on iteration), so `valid=false` yields an empty list.
pub(crate) async fn admin_list_tokens_route(
	State(services): State<crate::State>,
	body: Ruma<list::Request>,
) -> Result<list::Response> {
	require_admin(&services, body.sender_user()).await?;

	if body.valid == Some(false) {
		return Ok(list::Response { registration_tokens: Vec::new() });
	}

	let registration_tokens = services
		.registration_tokens
		.iterate_tokens()
		.map(valid_token_response)
		.collect()
		.await;

	Ok(list::Response { registration_tokens })
}
