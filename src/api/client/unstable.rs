use axum::extract::State;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use futures::StreamExt;
use ruma::{
	OwnedRoomId, UserId,
	api::client::membership::get_mutual_rooms::{msc2666, v1},
};
use tuwunel_core::{Err, Result, err, utils::math::ruma_from_usize_saturating};
use tuwunel_service::Services;

use crate::{ClientIp, Ruma};

/// Maximum number of rooms returned in a single `mutual_rooms` page.
const PAGE_SIZE: usize = 1000;

/// # `GET /_matrix/client/v1/mutual_rooms`
///
/// Gets all the rooms the sender shares with the specified user.
#[tracing::instrument(skip_all, fields(%client), name = "mutual_rooms")]
pub(crate) async fn get_mutual_rooms_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<v1::Request>,
) -> Result<v1::Response> {
	mutual_rooms(&services, body.sender_user(), &body.user_id, body.from.as_deref()).await
}

/// # `GET /_matrix/client/unstable/uk.half-shot.msc2666/user/mutual_rooms`
///
/// Gets shared rooms without the stable endpoint's total count.
#[tracing::instrument(skip_all, fields(%client), name = "mutual_rooms")]
pub(crate) async fn get_mutual_rooms_unstable_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<msc2666::Request>,
) -> Result<msc2666::Response> {
	let v1::Response { joined, next_batch, .. } =
		mutual_rooms(&services, body.sender_user(), &body.user_id, body.from.as_deref()).await?;

	Ok(msc2666::Response { joined, next_batch })
}

async fn mutual_rooms(
	services: &Services,
	sender_user: &UserId,
	user_id: &UserId,
	from: Option<&str>,
) -> Result<v1::Response> {
	if sender_user == user_id {
		return Err!(Request(InvalidParam("You cannot request rooms in common with yourself.")));
	}

	if user_id.validate_historical().is_err() {
		return Err!(Request(InvalidParam("The user_id is not a compliant user identifier.")));
	}

	let all: Vec<OwnedRoomId> = services
		.state_cache
		.get_shared_rooms(sender_user, user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let count = ruma_from_usize_saturating(all.len());

	let start = match from {
		| None => 0,
		| Some(token) => {
			let cursor = decode_cursor(token)
				.ok_or_else(|| err!(Request(InvalidParam("Invalid `from` token."))))?;

			all.partition_point(|room_id| room_id.as_str() <= cursor.as_str())
		},
	};

	let end = start.saturating_add(PAGE_SIZE).min(all.len());
	let next_batch = (end < all.len()).then(|| b64.encode(all[end.saturating_sub(1)].as_str()));

	let joined = if start == 0 && end == all.len() {
		all
	} else {
		all[start..end].to_vec()
	};

	Ok(v1::Response { joined, count, next_batch })
}

/// Decodes a base64url pagination cursor to its room id.
fn decode_cursor(token: &str) -> Option<OwnedRoomId> {
	let bytes = b64.decode(token).ok()?;
	let room_id = str::from_utf8(&bytes).ok()?;

	OwnedRoomId::parse(room_id).ok()
}
