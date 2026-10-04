use std::{
	mem::take,
	time::{Duration, SystemTime},
};

use axum::{Json, extract::State, response::IntoResponse};
use ruma::{
	MilliSecondsSinceUnixEpoch, Signatures,
	api::{
		OutgoingResponse,
		federation::discovery::{OldVerifyKey, ServerSigningKeys, get_server_keys},
	},
	serde::Raw,
};
use tuwunel_core::{Result, utils::timepoint_from_now};

/// # `GET /_matrix/key/v2/server`
///
/// Gets the public signing keys of this server.
///
/// - Matrix does not support invalidating public keys, so the key returned by
///   this will be valid forever.
// Response type for this endpoint is Json because we need to calculate a
// signature for the response
pub(crate) async fn get_server_keys_route(
	State(services): State<crate::State>,
) -> Result<impl IntoResponse> {
	let server_name = services.globals.server_name();
	let active_key_id = services.server_keys.active_key_id();
	let mut all_keys = services
		.server_keys
		.verify_keys_for(server_name)
		.await;

	let verify_keys = all_keys
		.remove_entry(active_key_id)
		.expect("active verify_key is missing");

	let old_verify_keys = all_keys
		.into_iter()
		.map(|(id, key)| (id, OldVerifyKey::new(expires_ts(), key.key)))
		.collect();

	let server_key = ServerSigningKeys {
		verify_keys: [verify_keys].into(),
		old_verify_keys,
		server_name: server_name.to_owned(),
		valid_until_ts: valid_until_ts(),
		signatures: Signatures::new(),
	};

	let server_key = Raw::new(&server_key)?;
	let mut response = get_server_keys::v2::Response::new(server_key)
		.try_into_http_response::<Vec<u8>>()
		.map(|mut response| take(response.body_mut()))
		.and_then(|body| serde_json::from_slice(&body).map_err(Into::into))?;

	services.server_keys.sign_json(&mut response)?;

	Ok(Json(response))
}

fn valid_until_ts() -> MilliSecondsSinceUnixEpoch {
	let dur = Duration::from_hours(168);
	let timepoint = timepoint_from_now(dur).expect("SystemTime should not overflow");
	MilliSecondsSinceUnixEpoch::from_system_time(timepoint).expect("UInt should not overflow")
}

fn expires_ts() -> MilliSecondsSinceUnixEpoch {
	let timepoint = SystemTime::now();
	MilliSecondsSinceUnixEpoch::from_system_time(timepoint).expect("UInt should not overflow")
}
