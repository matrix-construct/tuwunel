use std::time::{Duration, SystemTime};

use axum::extract::State;
use ruma::{JsOption, MilliSecondsSinceUnixEpoch, UInt};
use synapse_admin_api::registration_tokens::update::v1 as update;
use tuwunel_core::{Err, Result, utils::time::timepoint_from_epoch};
use tuwunel_service::registration_tokens::TokenExpires;

use super::token_response;
use crate::{Ruma, client::admin::require_admin};

/// # `PUT /_synapse/admin/v1/registration_tokens/{token}`
///
/// Only the cap and expiry are updatable; an omitted field is left unchanged
/// and an explicit `null` clears it.
pub(crate) async fn admin_update_token_route(
	State(services): State<crate::State>,
	body: Ruma<update::Request>,
) -> Result<update::Response> {
	require_admin(&services, body.sender_user()).await?;

	let token = &body.token;

	let Some(info) = services
		.registration_tokens
		.get_token_info(token)
		.await?
	else {
		return Err!(Request(NotFound("No such registration token")));
	};

	let max_uses = apply_uses(body.uses_allowed, info.expires.max_uses);
	let max_age = apply_age(body.expiry_time, info.expires.max_age)?;
	let expires = TokenExpires { max_uses, max_age };

	let info = services
		.registration_tokens
		.update_token(token, expires)
		.await?;

	Ok(update::Response {
		token: token_response(token.clone(), &info),
	})
}

/// Fold the tri-state `uses_allowed` over the stored cap: an undefined field
/// leaves it unchanged, an explicit `null` clears it.
fn apply_uses(uses_allowed: JsOption<UInt>, current: Option<u64>) -> Option<u64> {
	match uses_allowed.into_nested_option() {
		| None => current,
		| Some(cap) => cap.map(Into::into),
	}
}

/// Fold the tri-state `expiry_time` over the stored expiry, converting a set
/// millisecond timestamp into a `SystemTime`.
fn apply_age(
	expiry_time: JsOption<MilliSecondsSinceUnixEpoch>,
	current: Option<SystemTime>,
) -> Result<Option<SystemTime>> {
	match expiry_time.into_nested_option() {
		| None => Ok(current),
		| Some(None) => Ok(None),
		| Some(Some(ms)) => timepoint_from_epoch(Duration::from_millis(ms.0.into())).map(Some),
	}
}
