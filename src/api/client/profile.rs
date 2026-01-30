use std::collections::BTreeMap;

use axum::extract::State;
use futures::{
	StreamExt,
	future::{join, join4},
};
use ruma::{
	OwnedRoomId,
	api::{
		client::profile::{
			get_avatar_url, get_display_name, get_profile, set_avatar_url, set_display_name,
		},
		federation,
	},
	presence::PresenceState,
};
use tuwunel_core::{Err, Result, utils::future::TryExtExt};

use crate::Ruma;

/// # `PUT /_matrix/client/r0/profile/{userId}/displayname`
///
/// Updates the displayname.
///
/// - Also makes sure other users receive the update using presence EDUs
pub(crate) async fn set_displayname_route(
	State(services): State<crate::State>,
	body: Ruma<set_display_name::v3::Request>,
) -> Result<set_display_name::v3::Response> {
	let sender_user = body.sender_user();

	if *sender_user != body.user_id && body.appservice_info.is_none() {
		return Err!(Request(Forbidden("You cannot update the profile of another user")));
	}

	let all_joined_rooms: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_joined(&body.user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	services
		.users
		.update_displayname(&body.user_id, body.displayname.as_deref(), &all_joined_rooms)
		.await;

	// Presence update
	services
		.presence
		.maybe_ping_presence(&body.user_id, body.sender_device.as_deref(), &PresenceState::Online)
		.await?;

	Ok(set_display_name::v3::Response {})
}

/// # `GET /_matrix/client/v3/profile/{userId}/displayname`
///
/// Returns the displayname of the user.
///
/// - If user is on another server and we do not have a local copy already fetch
///   displayname over federation
pub(crate) async fn get_displayname_route(
	State(services): State<crate::State>,
	body: Ruma<get_display_name::v3::Request>,
) -> Result<get_display_name::v3::Response> {
	if !services.globals.user_is_local(&body.user_id) {
		// Create and update our local copy of the user
		if let Ok(response) = services
			.federation
			.execute(
				body.user_id.server_name(),
				federation::query::get_profile_information::v1::Request {
					user_id: body.user_id.clone(),
					field: None, // we want the full user's profile to update locally too
				},
			)
			.await
		{
			if !services.users.exists(&body.user_id).await {
				services
					.users
					.create(&body.user_id, None, None)
					.await?;
			}

			services
				.users
				.set_displayname(&body.user_id, response.displayname.as_deref());
			services
				.users
				.set_avatar_url(&body.user_id, response.avatar_url.as_deref());
			services
				.users
				.set_blurhash(&body.user_id, response.blurhash.as_deref());

			return Ok(get_display_name::v3::Response { displayname: response.displayname });
		}
	}

	if !services.users.exists(&body.user_id).await {
		// Return 404 if this user doesn't exist and we couldn't fetch it over
		// federation
		return Err!(Request(NotFound("Profile was not found.")));
	}

	Ok(get_display_name::v3::Response {
		displayname: services
			.users
			.displayname(&body.user_id)
			.await
			.ok(),
	})
}

/// # `PUT /_matrix/client/v3/profile/{userId}/avatar_url`
///
/// Updates the `avatar_url` and `blurhash`.
///
/// - Also makes sure other users receive the update using presence EDUs
pub(crate) async fn set_avatar_url_route(
	State(services): State<crate::State>,
	body: Ruma<set_avatar_url::v3::Request>,
) -> Result<set_avatar_url::v3::Response> {
	let sender_user = body.sender_user();

	if *sender_user != body.user_id && body.appservice_info.is_none() {
		return Err!(Request(Forbidden("You cannot update the profile of another user")));
	}

	let all_joined_rooms: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_joined(&body.user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	services
		.users
		.update_avatar_url(
			&body.user_id,
			body.avatar_url.as_deref(),
			body.blurhash.as_deref(),
			&all_joined_rooms,
		)
		.await;

	// Presence update
	services
		.presence
		.maybe_ping_presence(&body.user_id, body.sender_device.as_deref(), &PresenceState::Online)
		.await
		.ok();

	Ok(set_avatar_url::v3::Response {})
}

/// # `GET /_matrix/client/v3/profile/{userId}/avatar_url`
///
/// Returns the `avatar_url` and `blurhash` of the user.
///
/// - If user is on another server and we do not have a local copy already fetch
///   `avatar_url` and blurhash over federation
pub(crate) async fn get_avatar_url_route(
	State(services): State<crate::State>,
	body: Ruma<get_avatar_url::v3::Request>,
) -> Result<get_avatar_url::v3::Response> {
	if !services.globals.user_is_local(&body.user_id) {
		// Create and update our local copy of the user
		if let Ok(response) = services
			.federation
			.execute(
				body.user_id.server_name(),
				federation::query::get_profile_information::v1::Request {
					user_id: body.user_id.clone(),
					field: None, // we want the full user's profile to update locally as well
				},
			)
			.await
		{
			if !services.users.exists(&body.user_id).await {
				services
					.users
					.create(&body.user_id, None, None)
					.await?;
			}

			services
				.users
				.set_displayname(&body.user_id, response.displayname.as_deref());
			services
				.users
				.set_avatar_url(&body.user_id, response.avatar_url.as_deref());
			services
				.users
				.set_blurhash(&body.user_id, response.blurhash.as_deref());

			return Ok(get_avatar_url::v3::Response {
				avatar_url: response.avatar_url,
				blurhash: response.blurhash,
			});
		}
	}

	if !services.users.exists(&body.user_id).await {
		// Return 404 if this user doesn't exist and we couldn't fetch it over
		// federation
		return Err!(Request(NotFound("Profile was not found.")));
	}

	let (avatar_url, blurhash) = join(
		services.users.avatar_url(&body.user_id).ok(),
		services.users.blurhash(&body.user_id).ok(),
	)
	.await;

	Ok(get_avatar_url::v3::Response { avatar_url, blurhash })
}

/// # `GET /_matrix/client/v3/profile/{userId}`
///
/// Returns the displayname, avatar_url, blurhash, and tz of the user.
///
/// - If user is on another server and we do not have a local copy already,
///   fetch profile over federation.
pub(crate) async fn get_profile_route(
	State(services): State<crate::State>,
	body: Ruma<get_profile::v3::Request>,
) -> Result<get_profile::v3::Response> {
	if !services.globals.user_is_local(&body.user_id) {
		// Create and update our local copy of the user
		if let Ok(response) = services
			.federation
			.execute(
				body.user_id.server_name(),
				federation::query::get_profile_information::v1::Request {
					user_id: body.user_id.clone(),
					field: None,
				},
			)
			.await
		{
			if !services.users.exists(&body.user_id).await {
				services
					.users
					.create(&body.user_id, None, None)
					.await?;
			}

			services
				.users
				.set_displayname(&body.user_id, response.displayname.as_deref());
			services
				.users
				.set_avatar_url(&body.user_id, response.avatar_url.as_deref());
			services
				.users
				.set_blurhash(&body.user_id, response.blurhash.as_deref());
			services
				.users
				.set_timezone(&body.user_id, response.tz.as_deref());

			for (profile_key, profile_key_value) in &response.custom_profile_fields {
				services.users.set_profile_key(
					&body.user_id,
					profile_key,
					Some(profile_key_value),
				);
			}

			let canonical_fields = [
				("avatar_url", response.avatar_url.map(Into::into)),
				("blurhash", response.blurhash),
				("displayname", response.displayname),
				("m.tz", response.tz),
			];

			let response = canonical_fields
				.into_iter()
				.filter_map(|(key, val)| val.map(|val| (key, val)))
				.map(|(key, val)| (key.to_owned(), val.into()))
				.chain(response.custom_profile_fields.into_iter());

			return Ok(response.collect::<get_profile::v3::Response>());
		}
	}

	if !services.users.exists(&body.user_id).await {
		// Return 404 if this user doesn't exist and we couldn't fetch it over
		// federation
		return Err!(Request(NotFound("Profile was not found.")));
	}

	let mut custom_profile_fields: BTreeMap<String, serde_json::Value> = services
		.users
		.all_profile_keys(&body.user_id)
		.collect()
		.await;

	// services.users.timezone will collect the MSC4175 timezone key if it exists
	custom_profile_fields.remove("us.cloke.msc4175.tz");
	custom_profile_fields.remove("m.tz");

	let (avatar_url, blurhash, displayname, tz) = join4(
		services.users.avatar_url(&body.user_id).ok(),
		services.users.blurhash(&body.user_id).ok(),
		services.users.displayname(&body.user_id).ok(),
		services.users.timezone(&body.user_id).ok(),
	)
	.await;

	let canonical_fields = [
		("avatar_url", avatar_url.map(Into::into)),
		("blurhash", blurhash),
		("displayname", displayname),
		("m.tz", tz),
	];

	let response = canonical_fields
		.into_iter()
		.filter_map(|(key, val)| val.map(|val| (key, val)))
		.map(|(key, val)| (key.to_owned(), val.into()))
		.chain(custom_profile_fields.into_iter());

	Ok(response.collect::<get_profile::v3::Response>())
}
