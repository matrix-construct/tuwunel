use std::time::{Duration, SystemTime};

use axum::extract::State;
use base64::{Engine as _, engine::general_purpose};
use hmac::{Hmac, Mac};
use ruma::{SecondsSinceUnixEpoch, UserId, api::client::voip::get_turn_server_info};
use sha1::Sha1;
use tuwunel_core::{Err, Result, utils};

use crate::Ruma;

const RANDOM_USER_ID_LENGTH: usize = 10;

type HmacSha1 = Hmac<Sha1>;

/// # `GET /_matrix/client/r0/voip/turnServer`
///
/// TODO: Returns information about the recommended turn server.
pub(crate) async fn turn_server_route(
	State(services): State<crate::State>,
	body: Ruma<get_turn_server_info::v3::Request>,
) -> Result<get_turn_server_info::v3::Response> {
	// MSC4166: return M_NOT_FOUND 404 if no TURN URIs are specified in any way
	if services.server.config.turn_uris.is_empty() {
		return Err!(Request(NotFound("Not Found")));
	}

	let turn_secret = &services.globals.turn_secret;

	let (username, password) = if let Some(turn_secret) = turn_secret {
		let expiry = SecondsSinceUnixEpoch::from_system_time(
			SystemTime::now()
				.checked_add(Duration::from_secs(services.config.turn_ttl))
				.expect("TURN TTL should not get this high"),
		)
		.expect("time is valid");

		let user = body.sender_user.unwrap_or_else(|| {
			UserId::parse_with_server_name(
				utils::random_string(RANDOM_USER_ID_LENGTH).to_lowercase(),
				&services.server.name,
			)
			.unwrap()
		});

		let username: String = format!("{}:{}", expiry.get(), user);

		let mut mac = HmacSha1::new_from_slice(turn_secret.as_bytes())
			.expect("HMAC can take key of any size");
		mac.update(username.as_bytes());

		let password: String = general_purpose::STANDARD.encode(mac.finalize().into_bytes());

		(username, password)
	} else {
		(services.config.turn_username.clone(), services.config.turn_password.clone())
	};

	Ok(get_turn_server_info::v3::Response {
		username,
		password,
		uris: services.config.turn_uris.clone(),
		ttl: Duration::from_secs(services.config.turn_ttl),
	})
}
