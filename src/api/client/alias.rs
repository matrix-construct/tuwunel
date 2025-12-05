use axum::extract::State;
use futures::StreamExt;
use rand::seq::SliceRandom;
use ruma::{
	OwnedServerName, RoomAliasId, RoomId,
	api::client::alias::{create_alias, delete_alias, get_alias},
};
use tuwunel_core::{Err, Result, debug, err};
use tuwunel_service::Services;

use crate::Ruma;

/// # `PUT /_matrix/client/v3/directory/room/{roomAlias}`
///
/// Creates a new room alias on this server.
pub(crate) async fn create_alias_route(
	State(services): State<crate::State>,
	body: Ruma<create_alias::v3::Request>,
) -> Result<create_alias::v3::Response> {
	let sender_user = body.sender_user();
	services
		.alias
		.appservice_checks(&body.room_alias, &body.appservice_info)
		.await?;

	// this isn't apart of alias_checks or delete alias route because we should
	// allow removing forbidden room aliases
	if services
		.config
		.forbidden_alias_names
		.is_match(body.room_alias.alias())
	{
		return Err!(Request(Forbidden("Room alias is forbidden.")));
	}

	if services
		.alias
		.resolve_local_alias(&body.room_alias)
		.await
		.is_ok()
	{
		return Err!(Conflict("Alias already exists."));
	}

	services
		.alias
		.set_alias(&body.room_alias, &body.room_id, sender_user)?;

	Ok(create_alias::v3::Response::new())
}

/// # `DELETE /_matrix/client/v3/directory/room/{roomAlias}`
///
/// Deletes a room alias from this server.
///
/// - TODO: Update canonical alias event
pub(crate) async fn delete_alias_route(
	State(services): State<crate::State>,
	body: Ruma<delete_alias::v3::Request>,
) -> Result<delete_alias::v3::Response> {
	let sender_user = body.sender_user();
	services
		.alias
		.appservice_checks(&body.room_alias, &body.appservice_info)
		.await?;

	services
		.alias
		.remove_alias(&body.room_alias, sender_user)
		.await?;

	// TODO: update alt_aliases?

	Ok(delete_alias::v3::Response::new())
}

/// # `GET /_matrix/client/v3/directory/room/{roomAlias}`
///
/// Resolve an alias locally or over federation.
pub(crate) async fn get_alias_route(
	State(services): State<crate::State>,
	body: Ruma<get_alias::v3::Request>,
) -> Result<get_alias::v3::Response> {
	let room_alias = body.body.room_alias;

	let (room_id, servers) = services
		.alias
		.resolve_alias(&room_alias)
		.await
		.map_err(|_| err!(Request(NotFound("Room with alias not found."))))?;

	let servers = room_available_servers(&services, &room_id, &room_alias, servers).await;
	debug!(?room_alias, ?room_id, "available servers: {servers:?}");

	Ok(get_alias::v3::Response::new(room_id, servers))
}

async fn room_available_servers(
	services: &Services,
	room_id: &RoomId,
	room_alias: &RoomAliasId,
	pre_servers: Vec<OwnedServerName>,
) -> Vec<OwnedServerName> {
	// find active servers in room state cache to suggest
	let mut servers: Vec<OwnedServerName> = services
		.state_cache
		.room_servers(room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	// push any servers we want in the list already (e.g. responded remote alias
	// servers, room alias server itself)
	servers.extend(pre_servers);

	servers.sort_unstable();
	servers.dedup();

	// shuffle list of servers randomly after sort and dedupe
	servers.shuffle(&mut rand::thread_rng());

	// insert our server as the very first choice if in list, else check if we can
	// prefer the room alias server first
	if let Some(server_index) = servers
		.iter()
		.position(|server_name| services.globals.server_is_ours(server_name))
	{
		servers.swap(0, server_index);
	} else if let Some(alias_server_index) = servers
		.iter()
		.position(|server| server == room_alias.server_name())
	{
		servers.swap(0, alias_server_index);
	}

	servers
}
