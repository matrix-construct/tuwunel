use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use futures::StreamExt;
use ruma::{
	OwnedRoomId,
	api::{
		client::{
			error::ErrorKind,
			membership::mutual_rooms,
			profile::{
				ProfileFieldName, ProfileFieldValue, delete_profile_field, delete_timezone_key,
				get_profile_field, get_timezone_key, set_profile_field, set_timezone_key,
			},
		},
		federation,
	},
	presence::PresenceState,
};
use tuwunel_core::{Err, Error, Result, err};

use crate::Ruma;

/// # `GET /_matrix/client/unstable/uk.half-shot.msc2666/user/mutual_rooms`
///
/// Gets all the rooms the sender shares with the specified user.
///
/// TODO: Implement pagination, currently this just returns everything
///
/// An implementation of [MSC2666](https://github.com/matrix-org/matrix-spec-proposals/pull/2666)
#[tracing::instrument(skip_all, fields(%client), name = "mutual_rooms")]
pub(crate) async fn get_mutual_rooms_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<mutual_rooms::unstable::Request>,
) -> Result<mutual_rooms::unstable::Response> {
	let sender_user = body.sender_user();

	if sender_user == body.user_id {
		return Err!(Request(Unknown("You cannot request rooms in common with yourself.")));
	}

	if !services.users.exists(&body.user_id).await {
		return Ok(mutual_rooms::unstable::Response { joined: vec![], next_batch_token: None });
	}

	let mutual_rooms: Vec<OwnedRoomId> = services
		.state_cache
		.get_shared_rooms(sender_user, &body.user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	Ok(mutual_rooms::unstable::Response {
		joined: mutual_rooms,
		next_batch_token: None,
	})
}

/// # `DELETE /_matrix/client/unstable/uk.tcpip.msc4133/profile/{user_id}/us.cloke.msc4175.tz`
///
/// Deletes the `tz` (timezone) of a user, as per MSC4133 and MSC4175.
///
/// - Also makes sure other users receive the update using presence EDUs
pub(crate) async fn delete_timezone_key_route(
	State(services): State<crate::State>,
	body: Ruma<delete_timezone_key::unstable::Request>,
) -> Result<delete_timezone_key::unstable::Response> {
	let sender_user = body.sender_user();

	if *sender_user != body.user_id && body.appservice_info.is_none() {
		return Err!(Request(Forbidden("You cannot update the profile of another user")));
	}

	services.users.set_timezone(&body.user_id, None);

	// Presence update
	services
		.presence
		.maybe_ping_presence(&body.user_id, body.sender_device.as_deref(), &PresenceState::Online)
		.await?;

	Ok(delete_timezone_key::unstable::Response {})
}

/// # `PUT /_matrix/client/unstable/uk.tcpip.msc4133/profile/{user_id}/us.cloke.msc4175.tz`
///
/// Updates the `tz` (timezone) of a user, as per MSC4133 and MSC4175.
///
/// - Also makes sure other users receive the update using presence EDUs
pub(crate) async fn set_timezone_key_route(
	State(services): State<crate::State>,
	body: Ruma<set_timezone_key::unstable::Request>,
) -> Result<set_timezone_key::unstable::Response> {
	let sender_user = body.sender_user();

	if *sender_user != body.user_id && body.appservice_info.is_none() {
		return Err!(Request(Forbidden("You cannot update the profile of another user")));
	}

	services
		.users
		.set_timezone(&body.user_id, body.tz.as_deref());

	// Presence update
	services
		.presence
		.maybe_ping_presence(&body.user_id, body.sender_device.as_deref(), &PresenceState::Online)
		.await?;

	Ok(set_timezone_key::unstable::Response {})
}

/// # `PUT /_matrix/client/unstable/uk.tcpip.msc4133/profile/{user_id}/{field}`
///
/// Updates the profile key-value field of a user, as per MSC4133.
///
/// This also handles the avatar_url and displayname being updated.
pub(crate) async fn set_profile_field_route(
	State(services): State<crate::State>,
	body: Ruma<set_profile_field::v3::Request>,
) -> Result<set_profile_field::v3::Response> {
	let sender_user = body.sender_user();

	if *sender_user != body.user_id && body.appservice_info.is_none() {
		return Err!(Request(Forbidden("You cannot update the profile of another user")));
	}

	if body.value.field_name().as_str().len() > 128 {
		return Err!(Request(BadJson("Key names cannot be longer than 128 bytes")));
	}

	match &body.value {
		| ProfileFieldValue::DisplayName(displayname) => {
			let all_joined_rooms: Vec<OwnedRoomId> = services
				.state_cache
				.rooms_joined(&body.user_id)
				.map(Into::into)
				.collect()
				.await;

			services
				.users
				.update_displayname(&body.user_id, Some(displayname), &all_joined_rooms)
				.await;
		},
		| ProfileFieldValue::AvatarUrl(avatar_url) => {
			let all_joined_rooms: Vec<OwnedRoomId> = services
				.state_cache
				.rooms_joined(&body.user_id)
				.map(Into::into)
				.collect()
				.await;

			services
				.users
				.update_avatar_url(&body.user_id, Some(avatar_url), None, &all_joined_rooms)
				.await;
		},
		| _ => {
			services.users.set_profile_key(
				&body.user_id,
				body.value.field_name().as_str(),
				Some(&body.value.value()),
			);
		},
	}

	// Presence update
	services
		.presence
		.maybe_ping_presence(&body.user_id, body.sender_device.as_deref(), &PresenceState::Online)
		.await?;

	Ok(set_profile_field::v3::Response {})
}

/// # `DELETE /_matrix/client/unstable/uk.tcpip.msc4133/profile/{user_id}/{field}`
///
/// Deletes the profile key-value field of a user, as per MSC4133.
///
/// This also handles the avatar_url and displayname being updated.
pub(crate) async fn delete_profile_field_route(
	State(services): State<crate::State>,
	body: Ruma<delete_profile_field::v3::Request>,
) -> Result<delete_profile_field::v3::Response> {
	let sender_user = body.sender_user();

	if *sender_user != body.user_id && body.appservice_info.is_none() {
		return Err!(Request(Forbidden("You cannot update the profile of another user")));
	}

	match body.field {
		| ProfileFieldName::DisplayName => {
			let all_joined_rooms: Vec<OwnedRoomId> = services
				.state_cache
				.rooms_joined(&body.user_id)
				.map(Into::into)
				.collect()
				.await;

			services
				.users
				.update_displayname(&body.user_id, None, &all_joined_rooms)
				.await;
		},
		| ProfileFieldName::AvatarUrl => {
			let all_joined_rooms: Vec<OwnedRoomId> = services
				.state_cache
				.rooms_joined(&body.user_id)
				.map(Into::into)
				.collect()
				.await;

			services
				.users
				.update_avatar_url(&body.user_id, None, None, &all_joined_rooms)
				.await;
		},
		| _ => {
			services
				.users
				.set_profile_key(&body.user_id, body.field.as_str(), None);
		},
	}

	// Presence update
	services
		.presence
		.maybe_ping_presence(&body.user_id, body.sender_device.as_deref(), &PresenceState::Online)
		.await?;

	Ok(delete_profile_field::v3::Response {})
}

/// # `GET /_matrix/client/unstable/uk.tcpip.msc4133/profile/{user_id}/us.cloke.msc4175.tz`
///
/// Returns the `timezone` of the user as per MSC4133 and MSC4175.
///
/// - If user is on another server and we do not have a local copy already fetch
///   `timezone` over federation
pub(crate) async fn get_timezone_key_route(
	State(services): State<crate::State>,
	body: Ruma<get_timezone_key::unstable::Request>,
) -> Result<get_timezone_key::unstable::Response> {
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

			services
				.users
				.set_timezone(&body.user_id, response.tz.as_deref());

			return Ok(get_timezone_key::unstable::Response { tz: response.tz });
		}
	}

	if !services.users.exists(&body.user_id).await {
		// Return 404 if this user doesn't exist and we couldn't fetch it over
		// federation
		return Err(Error::BadRequest(ErrorKind::NotFound, "Profile was not found."));
	}

	Ok(get_timezone_key::unstable::Response {
		tz: services.users.timezone(&body.user_id).await.ok(),
	})
}

/// # `GET /_matrix/client/unstable/uk.tcpip.msc4133/profile/{userId}/{field}}`
///
/// Gets the profile key-value field of a user, as per MSC4133.
///
/// - If user is on another server and we do not have a local copy already fetch
///   `timezone` over federation
pub(crate) async fn get_profile_field_route(
	State(services): State<crate::State>,
	body: Ruma<get_profile_field::v3::Request>,
) -> Result<get_profile_field::v3::Response> {
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

			services
				.users
				.set_timezone(&body.user_id, response.tz.as_deref());

			let value = response
				.custom_profile_fields
				.get(body.field.as_str())
				.ok_or_else(|| {
					err!(Request(NotFound("The requested profile key does not exist.")))
				})?;

			services
				.users
				.set_profile_key(&body.user_id, body.field.as_str(), Some(value));

			let profile_key_value = ProfileFieldValue::new(body.field.as_str(), value.clone())?;

			return Ok(get_profile_field::v3::Response { value: Some(profile_key_value) });
		}
	}

	if !services.users.exists(&body.user_id).await {
		// Return 404 if this user doesn't exist and we couldn't fetch it over
		// federation
		return Err!(Request(NotFound("Profile was not found.")));
	}

	let value = services
		.users
		.profile_key(&body.user_id, body.field.as_str())
		.await
		.map_err(|_| err!(Request(NotFound("The requested profile key does not exist."))))?;

	let profile_key_value = ProfileFieldValue::new(body.field.as_str(), value)?;

	Ok(get_profile_field::v3::Response { value: Some(profile_key_value) })
}
