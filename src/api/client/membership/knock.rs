use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use ruma::api::client::knock::knock_room;
use tuwunel_core::Result;

use super::banned_room_check;
use crate::Ruma;

/// # `POST /_matrix/client/*/knock/{roomIdOrAlias}`
///
/// Tries to knock the room to ask permission to join for the sender user.
#[tracing::instrument(skip_all, fields(%client), name = "knock")]
pub(crate) async fn knock_room_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<knock_room::v3::Request>,
) -> Result<knock_room::v3::Response> {
	let sender_user = body.sender_user();

	let (room_id, servers) = services
		.alias
		.maybe_resolve_with_servers(&body.room_id_or_alias, Some(&body.via))
		.await?;

	banned_room_check(&services, sender_user, &room_id, Some(&body.room_id_or_alias), client)
		.await?;

	let state_lock = services.state.mutex.lock(&room_id).await;

	services
		.membership
		.knock(
			sender_user,
			&room_id,
			Some(&body.room_id_or_alias),
			body.reason.clone(),
			&servers,
			&state_lock,
		)
		.await?;

	drop(state_lock);

	Ok(knock_room::v3::Response::new(room_id.clone()))
}
