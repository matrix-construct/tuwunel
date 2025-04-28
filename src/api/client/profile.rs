use std::collections::BTreeMap;

use axum::extract::State;
use futures::{StreamExt, TryStreamExt, future::join3};
use ruma::{
	OwnedMxcUri, OwnedRoomId, UserId,
	api::{
		client::{
			error::ErrorKind,
			profile::{
				get_avatar_url, get_display_name, get_profile, set_avatar_url, set_display_name,
			},
		},
		federation,
	},
	events::room::member::{MembershipState, RoomMemberEventContent},
	presence::PresenceState,
};
use tuwunel_core::{
	Err, Error, Result,
	matrix::pdu::PduBuilder,
	utils::{IterStream, stream::TryIgnore},
	warn,
};
use tuwunel_service::Services;

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
	let sender_user = body
		.sender_user
		.as_ref()
		.expect("user is authenticated");

	if *sender_user != body.user_id && body.appservice_info.is_none() {
		return Err!(Request(Forbidden("You cannot update the profile of another user")));
	}

	let all_joined_rooms: Vec<OwnedRoomId> = services
		.rooms
		.state_cache
		.rooms_joined(&body.user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	update_displayname(&services, &body.user_id, body.displayname.clone(), &all_joined_rooms)
		.await;

	if services.config.allow_local_presence {
		// Presence update
		services
			.presence
			.ping_presence(&body.user_id, &PresenceState::Online)
			.await?;
	}

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
			.sending
			.send_federation_request(
				body.user_id.server_name(),
				federation::query::get_profile_information::v1::Request {
					user_id: body.user_id.clone(),
					field: None, // we want the full user's profile to update locally too
				},
			)
			.await
		{
			if !services.users.exists(&body.user_id).await {
				services.users.create(&body.user_id, None)?;
			}

			services
				.users
				.set_displayname(&body.user_id, response.displayname.clone());
			services
				.users
				.set_avatar_url(&body.user_id, response.avatar_url.clone());
			services
				.users
				.set_blurhash(&body.user_id, response.blurhash.clone());

			return Ok(get_display_name::v3::Response { displayname: response.displayname });
		}
	}

	if !services.users.exists(&body.user_id).await {
		// Return 404 if this user doesn't exist and we couldn't fetch it over
		// federation
		return Err(Error::BadRequest(ErrorKind::NotFound, "Profile was not found."));
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
	let sender_user = body
		.sender_user
		.as_ref()
		.expect("user is authenticated");

	if *sender_user != body.user_id && body.appservice_info.is_none() {
		return Err!(Request(Forbidden("You cannot update the profile of another user")));
	}

	let all_joined_rooms: Vec<OwnedRoomId> = services
		.rooms
		.state_cache
		.rooms_joined(&body.user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	update_avatar_url(
		&services,
		&body.user_id,
		body.avatar_url.clone(),
		body.blurhash.clone(),
		&all_joined_rooms,
	)
	.await;

	if services.config.allow_local_presence {
		// Presence update
		services
			.presence
			.ping_presence(&body.user_id, &PresenceState::Online)
			.await
			.ok();
	}

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
			.sending
			.send_federation_request(
				body.user_id.server_name(),
				federation::query::get_profile_information::v1::Request {
					user_id: body.user_id.clone(),
					field: None, // we want the full user's profile to update locally as well
				},
			)
			.await
		{
			if !services.users.exists(&body.user_id).await {
				services.users.create(&body.user_id, None)?;
			}

			services
				.users
				.set_displayname(&body.user_id, response.displayname.clone());

			services
				.users
				.set_avatar_url(&body.user_id, response.avatar_url.clone());

			services
				.users
				.set_blurhash(&body.user_id, response.blurhash.clone());

			return Ok(get_avatar_url::v3::Response {
				avatar_url: response.avatar_url,
				blurhash: response.blurhash,
			});
		}
	}

	if !services.users.exists(&body.user_id).await {
		// Return 404 if this user doesn't exist and we couldn't fetch it over
		// federation
		return Err(Error::BadRequest(ErrorKind::NotFound, "Profile was not found."));
	}

	Ok(get_avatar_url::v3::Response {
		avatar_url: services
			.users
			.avatar_url(&body.user_id)
			.await
			.ok(),
		blurhash: services.users.blurhash(&body.user_id).await.ok(),
	})
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
			.sending
			.send_federation_request(
				body.user_id.server_name(),
				federation::query::get_profile_information::v1::Request {
					user_id: body.user_id.clone(),
					field: None,
				},
			)
			.await
		{
			if !services.users.exists(&body.user_id).await {
				services.users.create(&body.user_id, None)?;
			}

			services
				.users
				.set_displayname(&body.user_id, response.displayname.clone());

			services
				.users
				.set_avatar_url(&body.user_id, response.avatar_url.clone());

			services
				.users
				.set_blurhash(&body.user_id, response.blurhash.clone());

			services
				.users
				.set_timezone(&body.user_id, response.tz.clone());

			for (profile_key, profile_key_value) in &response.custom_profile_fields {
				services.users.set_profile_key(
					&body.user_id,
					profile_key,
					Some(profile_key_value.clone()),
				);
			}

			return Ok(get_profile::v3::Response {
				displayname: response.displayname,
				avatar_url: response.avatar_url,
				blurhash: response.blurhash,
				tz: response.tz,
				custom_profile_fields: response.custom_profile_fields,
			});
		}
	}

	if !services.users.exists(&body.user_id).await {
		// Return 404 if this user doesn't exist and we couldn't fetch it over
		// federation
		return Err(Error::BadRequest(ErrorKind::NotFound, "Profile was not found."));
	}

	let mut custom_profile_fields: BTreeMap<String, serde_json::Value> = services
		.users
		.all_profile_keys(&body.user_id)
		.collect()
		.await;

	// services.users.timezone will collect the MSC4175 timezone key if it exists
	custom_profile_fields.remove("us.cloke.msc4175.tz");
	custom_profile_fields.remove("m.tz");

	Ok(get_profile::v3::Response {
		avatar_url: services
			.users
			.avatar_url(&body.user_id)
			.await
			.ok(),
		blurhash: services.users.blurhash(&body.user_id).await.ok(),
		displayname: services
			.users
			.displayname(&body.user_id)
			.await
			.ok(),
		tz: services.users.timezone(&body.user_id).await.ok(),
		custom_profile_fields,
	})
}

pub async fn update_displayname(
	services: &Services,
	user_id: &UserId,
	displayname: Option<String>,
	all_joined_rooms: &[OwnedRoomId],
) {
	let (current_avatar_url, current_blurhash, current_displayname) = join3(
		services.users.avatar_url(user_id),
		services.users.blurhash(user_id),
		services.users.displayname(user_id),
	)
	.await;

	let current_avatar_url = current_avatar_url.ok();
	let current_blurhash = current_blurhash.ok();
	let current_displayname = current_displayname.ok();

	if displayname == current_displayname {
		return;
	}

	services
		.users
		.set_displayname(user_id, displayname.clone());

	// Send a new join membership event into all joined rooms
	let avatar_url = &current_avatar_url;
	let blurhash = &current_blurhash;
	let displayname = &displayname;
	let all_joined_rooms: Vec<_> = all_joined_rooms
		.iter()
		.try_stream()
		.and_then(|room_id: &OwnedRoomId| async move {
			let pdu = PduBuilder::state(
				user_id.to_string(),
				&RoomMemberEventContent {
					displayname: displayname.clone(),
					membership: MembershipState::Join,
					avatar_url: avatar_url.clone(),
					blurhash: blurhash.clone(),
					join_authorized_via_users_server: None,
					reason: None,
					is_direct: None,
					third_party_invite: None,
				},
			);

			Ok((pdu, room_id))
		})
		.ignore_err()
		.collect()
		.await;

	update_all_rooms(services, all_joined_rooms, user_id).await;
}

pub async fn update_avatar_url(
	services: &Services,
	user_id: &UserId,
	avatar_url: Option<OwnedMxcUri>,
	blurhash: Option<String>,
	all_joined_rooms: &[OwnedRoomId],
) {
	let (current_avatar_url, current_blurhash, current_displayname) = join3(
		services.users.avatar_url(user_id),
		services.users.blurhash(user_id),
		services.users.displayname(user_id),
	)
	.await;

	let current_avatar_url = current_avatar_url.ok();
	let current_blurhash = current_blurhash.ok();
	let current_displayname = current_displayname.ok();

	if current_avatar_url == avatar_url && current_blurhash == blurhash {
		return;
	}

	services
		.users
		.set_avatar_url(user_id, avatar_url.clone());
	services
		.users
		.set_blurhash(user_id, blurhash.clone());

	// Send a new join membership event into all joined rooms
	let avatar_url = &avatar_url;
	let blurhash = &blurhash;
	let displayname = &current_displayname;
	let all_joined_rooms: Vec<_> = all_joined_rooms
		.iter()
		.try_stream()
		.and_then(|room_id: &OwnedRoomId| async move {
			let pdu = PduBuilder::state(
				user_id.to_string(),
				&RoomMemberEventContent {
					avatar_url: avatar_url.clone(),
					blurhash: blurhash.clone(),
					membership: MembershipState::Join,
					displayname: displayname.clone(),
					join_authorized_via_users_server: None,
					reason: None,
					is_direct: None,
					third_party_invite: None,
				},
			);

			Ok((pdu, room_id))
		})
		.ignore_err()
		.collect()
		.await;

	update_all_rooms(services, all_joined_rooms, user_id).await;
}

pub async fn update_all_rooms(
	services: &Services,
	all_joined_rooms: Vec<(PduBuilder, &OwnedRoomId)>,
	user_id: &UserId,
) {
	for (pdu_builder, room_id) in all_joined_rooms {
		let state_lock = services.rooms.state.mutex.lock(room_id).await;
		if let Err(e) = services
			.rooms
			.timeline
			.build_and_append_pdu(pdu_builder, user_id, room_id, &state_lock)
			.await
		{
			warn!(%user_id, %room_id, "Failed to update/send new profile join membership update in room: {e}");
		}
	}
}
