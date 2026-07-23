use axum::extract::State;
use futures::StreamExt;
use ruma::{
	api::client::profile::{
		PropagateTo, delete_profile_field, get_profile, get_profile_field, set_profile_field,
	},
	profile::ProfileFieldValue,
};
use tuwunel_core::{Err, Result, err};
use tuwunel_service::{presence::Ping, profile::Propagation};

use crate::{ClientIp, Ruma};

/// Resolve a `PropagateTo` request value against the server default.
///
/// MSC4466's `_Custom` variant is treated as the server default so
/// unknown values do not silently change behavior.
pub(super) fn resolve_propagation(propagate_to: &PropagateTo) -> Propagation {
	match propagate_to {
		| PropagateTo::Unchanged => Propagation::Unchanged,
		| PropagateTo::None => Propagation::None,
		| _ => Propagation::All,
	}
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
		services
			.profile
			.fetch_remote_profile(&body.user_id)
			.await?;
	}

	if !services.users.exists(&body.user_id).await {
		// Return 404 if this user doesn't exist and we couldn't fetch it over
		// federation
		return Err!(Request(NotFound("Profile was not found.")));
	}

	let response = services
		.profile
		.all_profile_keys(&body.user_id)
		.collect()
		.await;

	Ok(response)
}

/// # `GET /_matrix/client/v3/profile/{userId}/{field}`
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
		services
			.profile
			.fetch_remote_profile(&body.user_id)
			.await?;
	}

	if !services.users.exists(&body.user_id).await {
		// Return 404 if this user doesn't exist and we couldn't fetch it over
		// federation
		return Err!(Request(NotFound("Profile was not found.")));
	}

	let value = services
		.profile
		.profile_key(&body.user_id, &body.field)
		.await?;

	let profile_value = ProfileFieldValue::new(body.field.as_str(), value).map_err(|_| {
		err!(Database(
			error!(user_id = %body.user_id, key = %body.field, "Invalid json in database profile value")
		))
	})?;

	Ok(get_profile_field::v3::Response { value: Some(profile_value) })
}

/// # `PUT /_matrix/client/v3/profile/{user_id}/{field}`
///
/// Updates the profile key-value field of a user. Stabilized as part of
/// Matrix 1.16 (MSC4133); ruma's history block keeps the unstable
/// `uk.tcpip.msc4133` path mounted for older clients.
///
/// This also handles the avatar_url and displayname being updated.
pub(crate) async fn set_profile_field_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<set_profile_field::v3::Request>,
) -> Result<set_profile_field::v3::Response> {
	let sender_user = body.sender_user();

	if *sender_user != body.user_id
		&& !body
			.appservice_info
			.as_ref()
			.is_some_and(|registration| registration.is_user_match(&body.user_id))
	{
		return Err!(Request(Forbidden("You cannot update the profile of another user")));
	}

	let propagation = resolve_propagation(&body.propagate_to);

	services
		.profile
		.set_profile_keys(
			&body.user_id,
			&[(body.value.field_name(), Some(body.value.value().into_owned()))],
			Some(propagation),
		)
		.await?;

	// Presence update
	let ping = Ping {
		device_id: body.sender_device.as_deref(),
		client_ip: Some(client),
		appservice: body.appservice_info.as_ref(),
		..Default::default()
	};

	services
		.presence
		.maybe_ping_presence(&body.user_id, ping)
		.await?;

	Ok(set_profile_field::v3::Response {})
}

/// # `DELETE /_matrix/client/v3/profile/{user_id}/{field}`
///
/// Deletes the profile key-value field of a user, as per MSC4133.
///
/// This also handles the avatar_url and displayname being updated.
pub(crate) async fn delete_profile_field_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<delete_profile_field::v3::Request>,
) -> Result<delete_profile_field::v3::Response> {
	let sender_user = body.sender_user();

	if *sender_user != body.user_id
		&& !body
			.appservice_info
			.as_ref()
			.is_some_and(|registration| registration.is_user_match(&body.user_id))
	{
		return Err!(Request(Forbidden("You cannot update the profile of another user")));
	}

	let propagation = resolve_propagation(&body.propagate_to);

	services
		.profile
		.set_profile_keys(&body.user_id, &[(body.field.clone(), None)], Some(propagation))
		.await?;

	// Presence update
	let ping = Ping {
		device_id: body.sender_device.as_deref(),
		client_ip: Some(client),
		appservice: body.appservice_info.as_ref(),
		..Default::default()
	};

	services
		.presence
		.maybe_ping_presence(&body.user_id, ping)
		.await?;

	Ok(delete_profile_field::v3::Response {})
}
