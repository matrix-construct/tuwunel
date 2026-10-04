use axum::extract::State;
use futures::StreamExt;
use rand::seq::SliceRandom;
use ruma::{
	OwnedServerName,
	api::federation::query::{get_profile_information, get_room_information},
	profile::ProfileFieldValue,
};
use tuwunel_core::{Err, Result, err};

use crate::Ruma;

/// # `GET /_matrix/federation/v1/query/directory`
///
/// Resolve a room alias to a room id.
pub(crate) async fn get_room_information_route(
	State(services): State<crate::State>,
	body: Ruma<get_room_information::v1::Request>,
) -> Result<get_room_information::v1::Response> {
	let room_id = services
		.alias
		.resolve_local_alias(&body.room_alias)
		.await
		.map_err(|_| err!(Request(NotFound("Room alias not found."))))?;

	let mut servers: Vec<OwnedServerName> = services
		.state_cache
		.room_servers(&room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	servers.sort_unstable();
	servers.dedup();

	servers.shuffle(&mut rand::rng());

	// insert our server as the very first choice if in list
	if let Some(server_index) = servers
		.iter()
		.position(|server| server == services.globals.server_name())
	{
		servers.swap_remove(server_index);
		servers.insert(0, services.globals.server_name().to_owned());
	}

	Ok(get_room_information::v1::Response { room_id, servers })
}

/// # `GET /_matrix/federation/v1/query/profile`
///
///
/// Gets information on a profile.
pub(crate) async fn get_profile_information_route(
	State(services): State<crate::State>,
	body: Ruma<get_profile_information::v1::Request>,
) -> Result<get_profile_information::v1::Response> {
	if !services
		.server
		.config
		.allow_inbound_profile_lookup_federation_requests
	{
		return Err!(Request(Forbidden(
			"Profile lookup over federation is not allowed on this homeserver.",
		)));
	}

	if !services.globals.user_is_local(&body.user_id) {
		return Err!(Request(InvalidParam("User does not belong to this server.",)));
	}

	if !services.users.exists(&body.user_id).await {
		return Err!(Request(NotFound("Profile was not found.")));
	}

	match &body.field {
		| Some(field) => {
			let value = services
				.profile
				.profile_key(&body.user_id, field)
				.await
				.ok();

			let response = value
				.map(|value| ProfileFieldValue::new(field.as_str(), value))
				.transpose()
				.map_err(|_| {
					err!(Database(
						error!(user_id = %body.user_id, key = %field, "Invalid json in database profile value")
					))
				})?
				.into_iter()
				.collect();

			Ok(response)
		},
		| None => {
			let response = services
				.profile
				.all_profile_keys(&body.user_id)
				.collect::<_>()
				.await;

			Ok(response)
		},
	}
}
