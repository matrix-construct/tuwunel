use axum::extract::State;
use futures::StreamExt;
use ruma::{UserId, api::client::keys::upload_signatures};
use serde_json::value::RawValue;
use tuwunel_core::{Result, debug, debug_warn, utils::IterStream};

use crate::Ruma;

/// # `POST /_matrix/client/r0/keys/signatures/upload`
///
/// Uploads end-to-end key signatures from the sender user.
pub(crate) async fn upload_signatures_route(
	State(services): State<crate::State>,
	body: Ruma<upload_signatures::v3::Request>,
) -> Result<upload_signatures::v3::Response> {
	let sender_user = body.sender_user();

	if body.signed_keys.is_empty() {
		debug!("Empty signed_keys sent in key signature upload");
		return Ok(upload_signatures::v3::Response::new());
	}

	body.signed_keys
		.iter()
		.flat_map(|(user_id, keys)| {
			keys.iter().flat_map(move |(key_id, key)| {
				signatures_from_key(sender_user, key_id, key)
					.map(move |sig| (user_id.as_ref(), key_id, sig))
			})
		})
		.stream()
		.for_each_concurrent(None, async |(user_id, key_id, signature)| {
			services
				.users
				.sign_key(user_id, key_id, signature, sender_user)
				.await
				.inspect_err(|e| debug_warn!("{e}"))
				.ok();
		})
		.await;

	Ok(upload_signatures::v3::Response::default())
}

fn signatures_from_key(
	sender_user: &UserId,
	key_id: &str,
	key: &RawValue,
) -> impl Iterator<Item = (String, String)> + Send + 'static {
	serde_json::to_value(key)
		.inspect_err(|e| debug_warn!(?key_id, "Invalid \"key\" JSON: {e}"))
		.ok()
		.and_then(|key| key.get("signatures").cloned())
		.and_then(|sigs| sigs.get(sender_user.to_string()).cloned())
		.and_then(|val| val.as_object().cloned())
		.into_iter()
		.flatten()
		.filter_map(|(sig_id, val)| Some((sig_id, val.as_str().map(ToOwned::to_owned)?)))
}
