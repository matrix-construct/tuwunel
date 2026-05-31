use axum::extract::State;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use futures::StreamExt;
use ruma::{OwnedRoomId, UInt, api::client::membership::mutual_rooms};
use tuwunel_core::{Err, Result, err};

use crate::{ClientIp, Ruma};

/// Maximum number of rooms returned in a single `mutual_rooms` page.
const PAGE_SIZE: usize = 1000;

/// # `GET /_matrix/client/v1/mutual_rooms`
/// # `GET /_matrix/client/unstable/uk.half-shot.msc2666/user/mutual_rooms`
///
/// Gets all the rooms the sender shares with the specified user.
///
/// An implementation of [MSC2666](https://github.com/matrix-org/matrix-spec-proposals/pull/2666)
#[tracing::instrument(skip_all, fields(%client), name = "mutual_rooms")]
pub(crate) async fn get_mutual_rooms_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<mutual_rooms::v1::Request>,
) -> Result<mutual_rooms::v1::Response> {
	let sender_user = body.sender_user();

	if sender_user == body.user_id {
		return Err!(Request(InvalidParam("You cannot request rooms in common with yourself.")));
	}

	if body.user_id.validate_historical().is_err() {
		return Err!(Request(InvalidParam("The user_id is not a compliant user identifier.")));
	}

	let all: Vec<OwnedRoomId> = services
		.state_cache
		.get_shared_rooms(sender_user, &body.user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let count = UInt::try_from(all.len()).unwrap_or(UInt::MAX);

	let start = match body.from.as_deref() {
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

	Ok(mutual_rooms::v1::Response { joined, count, next_batch })
}

/// Decodes a base64url pagination cursor to its room id.
fn decode_cursor(token: &str) -> Option<OwnedRoomId> {
	let bytes = b64.decode(token).ok()?;
	let room_id = str::from_utf8(&bytes).ok()?;

	OwnedRoomId::parse(room_id).ok()
}
