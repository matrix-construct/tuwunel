use axum::extract::State;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, RoomId,
	api::client::membership::{join_room_by_id, join_room_by_id_or_alias},
};
use tuwunel_core::{Result, warn};
use tuwunel_service::membership::Join;

use super::banned_room_check;
use crate::{ClientIp, Ruma};

/// # `POST /_matrix/client/r0/rooms/{roomId}/join`
///
/// Tries to join the sender user into a room.
///
/// - If the server knowns about this room: creates the join event and does auth
///   rules locally
/// - If the server does not know about the room: asks other servers over
///   federation
#[tracing::instrument(skip_all, fields(%client), name = "join")]
pub(crate) async fn join_room_by_id_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<join_room_by_id::v3::Request>,
) -> Result<join_room_by_id::v3::Response> {
	let sender_user = body.sender_user();

	let room_id: &RoomId = &body.room_id;

	banned_room_check(&services, sender_user, room_id, None, client).await?;

	let extra_content = extra_member_content(body.json_body.as_ref());

	let mut errors = 0_usize;
	while let Err(e) = services
		.membership
		.join(Join {
			sender_user,
			room_id,
			orig_room_id: None,
			reason: body.reason.clone(),
			servers: &[],
			is_appservice: body.appservice_info.is_some(),
			extra_content: extra_content.clone(),
		})
		.await
	{
		errors = errors.saturating_add(1);
		if errors >= services.config.max_join_attempts_per_join_request {
			warn!(
				"Several servers failed. Giving up for this request. Try again for different \
				 server selection."
			);
			return Err(e);
		}
	}

	Ok(join_room_by_id::v3::Response { room_id: room_id.to_owned() })
}

/// # `POST /_matrix/client/r0/join/{roomIdOrAlias}`
///
/// Tries to join the sender user into a room.
///
/// - If the server knowns about this room: creates the join event and does auth
///   rules locally
/// - If the server does not know about the room: use the server name query
///   param if specified. if not specified, asks other servers over federation
///   via room alias server name and room ID server name
#[tracing::instrument(skip_all, fields(%client), name = "join")]
pub(crate) async fn join_room_by_id_or_alias_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<join_room_by_id_or_alias::v3::Request>,
) -> Result<join_room_by_id_or_alias::v3::Response> {
	let sender_user = body.sender_user();
	let appservice_info = &body.appservice_info;

	let (room_id, servers) = services
		.alias
		.maybe_resolve_with_servers(&body.room_id_or_alias, Some(&body.via))
		.await?;

	banned_room_check(&services, sender_user, &room_id, Some(&body.room_id_or_alias), client)
		.await?;

	let extra_content = extra_member_content(body.json_body.as_ref());

	let mut errors = 0_usize;
	while let Err(e) = services
		.membership
		.join(Join {
			sender_user,
			room_id: &room_id,
			orig_room_id: Some(&body.room_id_or_alias),
			reason: body.reason.clone(),
			servers: &servers,
			is_appservice: appservice_info.is_some(),
			extra_content: extra_content.clone(),
		})
		.await
	{
		errors = errors.saturating_add(1);
		if errors >= services.config.max_join_attempts_per_join_request {
			warn!(
				"Several servers failed. Giving up for this request. Try again for different \
				 server selection."
			);
			return Err(e);
		}
	}

	Ok(join_room_by_id_or_alias::v3::Response { room_id: room_id.clone() })
}

const RESERVED_JOIN_KEYS: [&str; 3] =
	["reason", "third_party_signed", "join_authorised_via_users_server"];

// Drop recognized and server-owned keys the client must not set.
fn extra_member_content(json_body: Option<&CanonicalJsonValue>) -> Option<CanonicalJsonObject> {
	let CanonicalJsonValue::Object(object) = json_body? else {
		return None;
	};

	let extra: CanonicalJsonObject = object
		.iter()
		.filter(|(key, _)| !RESERVED_JOIN_KEYS.contains(&key.as_str()))
		.map(|(key, value)| (key.clone(), value.clone()))
		.collect();

	(!extra.is_empty()).then_some(extra)
}
