use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use futures::{FutureExt, StreamExt};
use ruma::{
	OwnedRoomId, UserId,
	api::client::{
		account::{
			ThirdPartyIdRemovalStatus, change_password, deactivate, get_3pids,
			request_3pid_management_token_via_email, request_3pid_management_token_via_msisdn,
			whoami,
		},
		uiaa::{AuthFlow, AuthType, UiaaInfo},
	},
	events::{
		StateEventType,
		room::power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
	},
};
use tuwunel_core::{
	Err, Error, Result, err, info,
	matrix::{Event, pdu::PduBuilder},
	utils,
	utils::{ReadyExt, stream::BroadbandExt},
	warn,
};
use tuwunel_service::Services;

use super::SESSION_ID_LENGTH;
use crate::Ruma;

/// # `POST /_matrix/client/r0/account/password`
///
/// Changes the password of this account.
///
/// - Requires UIAA to verify user password
/// - Changes the password of the sender user
/// - The password hash is calculated using argon2 with 32 character salt, the
///   plain password is
/// not saved
///
/// If logout_devices is true it does the following for each device except the
/// sender device:
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
#[tracing::instrument(skip_all, fields(%client), name = "change_password")]
pub(crate) async fn change_password_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<change_password::v3::Request>,
) -> Result<change_password::v3::Response> {
	// Authentication for this endpoint was made optional, but we need
	// authentication currently
	let sender_user = body
		.sender_user
		.as_ref()
		.ok_or_else(|| err!(Request(MissingToken("Missing access token."))))?;

	let mut uiaainfo = UiaaInfo {
		flows: vec![AuthFlow { stages: vec![AuthType::Password] }],
		completed: Vec::new(),
		params: Box::default(),
		session: None,
		auth_error: None,
	};

	match &body.auth {
		| Some(auth) => {
			let (worked, uiaainfo) = services
				.uiaa
				.try_auth(sender_user, body.sender_device(), auth, &uiaainfo)
				.await?;

			if !worked {
				return Err(Error::Uiaa(uiaainfo));
			}

			// Success!
		},
		| _ => match body.json_body {
			| Some(ref json) => {
				uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
				services
					.uiaa
					.create(sender_user, body.sender_device(), &uiaainfo, json);

				return Err(Error::Uiaa(uiaainfo));
			},
			| _ => {
				return Err!(Request(NotJson("JSON body is not valid")));
			},
		},
	}

	services
		.users
		.set_password(sender_user, Some(&body.new_password))
		.await?;

	if body.logout_devices {
		// Logout all devices except the current one
		services
			.users
			.all_device_ids(sender_user)
			.ready_filter(|id| *id != body.sender_device())
			.for_each(|id| services.users.remove_device(sender_user, id))
			.await;

		// Remove all pushers except the ones associated with this session
		services
			.pusher
			.get_pushkeys(sender_user)
			.map(ToOwned::to_owned)
			.broad_filter_map(async |pushkey| {
				services
					.pusher
					.get_pusher_device(&pushkey)
					.await
					.ok()
					.filter(|pusher_device| pusher_device != body.sender_device())
					.is_some()
					.then_some(pushkey)
			})
			.for_each(async |pushkey| {
				services
					.pusher
					.delete_pusher(sender_user, &pushkey)
					.await;
			})
			.await;
	}

	info!("User {sender_user} changed their password.");

	if services.server.config.admin_room_notices {
		services
			.admin
			.notice(&format!("User {sender_user} changed their password."))
			.await;
	}

	Ok(change_password::v3::Response {})
}

/// # `GET _matrix/client/r0/account/whoami`
///
/// Get `user_id` of the sender user.
///
/// Note: Also works for Application Services
pub(crate) async fn whoami_route(
	State(services): State<crate::State>,
	body: Ruma<whoami::v3::Request>,
) -> Result<whoami::v3::Response> {
	Ok(whoami::v3::Response {
		user_id: body.sender_user().to_owned(),
		device_id: body.sender_device.clone(),
		is_guest: services
			.users
			.is_deactivated(body.sender_user())
			.await? && body.appservice_info.is_none(),
	})
}

/// # `POST /_matrix/client/r0/account/deactivate`
///
/// Deactivate sender user account.
///
/// - Leaves all rooms and rejects all invitations
/// - Invalidates all access tokens
/// - Deletes all device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets all to-device events
/// - Triggers device list updates
/// - Removes ability to log in again
#[tracing::instrument(skip_all, fields(%client), name = "deactivate")]
pub(crate) async fn deactivate_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<deactivate::v3::Request>,
) -> Result<deactivate::v3::Response> {
	// Authentication for this endpoint was made optional, but we need
	// authentication currently
	let sender_user = body
		.sender_user
		.as_ref()
		.ok_or_else(|| err!(Request(MissingToken("Missing access token."))))?;

	let mut uiaainfo = UiaaInfo {
		flows: vec![AuthFlow { stages: vec![AuthType::Password] }],
		completed: Vec::new(),
		params: Box::default(),
		session: None,
		auth_error: None,
	};

	match &body.auth {
		| Some(auth) => {
			let (worked, uiaainfo) = services
				.uiaa
				.try_auth(sender_user, body.sender_device(), auth, &uiaainfo)
				.await?;

			if !worked {
				return Err(Error::Uiaa(uiaainfo));
			}
			// Success!
		},
		| _ => match body.json_body {
			| Some(ref json) => {
				uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
				services
					.uiaa
					.create(sender_user, body.sender_device(), &uiaainfo, json);

				return Err(Error::Uiaa(uiaainfo));
			},
			| _ => {
				return Err!(Request(NotJson("JSON body is not valid")));
			},
		},
	}

	// Remove profile pictures and display name
	let all_joined_rooms: Vec<OwnedRoomId> = services
		.rooms
		.state_cache
		.rooms_joined(sender_user)
		.map(Into::into)
		.collect()
		.await;

	super::update_displayname(&services, sender_user, None, &all_joined_rooms).await;
	super::update_avatar_url(&services, sender_user, None, None, &all_joined_rooms).await;

	full_user_deactivate(&services, sender_user, &all_joined_rooms)
		.boxed()
		.await?;

	info!("User {sender_user} deactivated their account.");

	if services.server.config.admin_room_notices {
		services
			.admin
			.notice(&format!("User {sender_user} deactivated their account."))
			.await;
	}

	Ok(deactivate::v3::Response {
		id_server_unbind_result: ThirdPartyIdRemovalStatus::NoSupport,
	})
}

/// # `GET _matrix/client/v3/account/3pid`
///
/// Get a list of third party identifiers associated with this account.
///
/// - Currently always returns empty list
pub(crate) async fn third_party_route(
	body: Ruma<get_3pids::v3::Request>,
) -> Result<get_3pids::v3::Response> {
	let _sender_user = body
		.sender_user
		.as_ref()
		.expect("user is authenticated");

	Ok(get_3pids::v3::Response::new(Vec::new()))
}

/// # `POST /_matrix/client/v3/account/3pid/email/requestToken`
///
/// "This API should be used to request validation tokens when adding an email
/// address to an account"
///
/// - 403 signals that The homeserver does not allow the third party identifier
///   as a contact option.
pub(crate) async fn request_3pid_management_token_via_email_route(
	_body: Ruma<request_3pid_management_token_via_email::v3::Request>,
) -> Result<request_3pid_management_token_via_email::v3::Response> {
	Err!(Request(ThreepidDenied("Third party identifiers are not implemented")))
}

/// # `POST /_matrix/client/v3/account/3pid/msisdn/requestToken`
///
/// "This API should be used to request validation tokens when adding an phone
/// number to an account"
///
/// - 403 signals that The homeserver does not allow the third party identifier
///   as a contact option.
pub(crate) async fn request_3pid_management_token_via_msisdn_route(
	_body: Ruma<request_3pid_management_token_via_msisdn::v3::Request>,
) -> Result<request_3pid_management_token_via_msisdn::v3::Response> {
	Err!(Request(ThreepidDenied("Third party identifiers are not implemented")))
}

/// Runs through all the deactivation steps:
///
/// - Mark as deactivated
/// - Removing display name
/// - Removing avatar URL and blurhash
/// - Removing all profile data
/// - Leaving all rooms (and forgets all of them)
pub async fn full_user_deactivate(
	services: &Services,
	user_id: &UserId,
	all_joined_rooms: &[OwnedRoomId],
) -> Result {
	services
		.users
		.deactivate_account(user_id)
		.await
		.ok();

	super::update_displayname(services, user_id, None, all_joined_rooms).await;
	super::update_avatar_url(services, user_id, None, None, all_joined_rooms).await;

	services
		.users
		.all_profile_keys(user_id)
		.ready_for_each(|(profile_key, _)| {
			services
				.users
				.set_profile_key(user_id, &profile_key, None);
		})
		.await;

	for room_id in all_joined_rooms {
		let state_lock = services.rooms.state.mutex.lock(room_id).await;

		let room_power_levels = services
			.rooms
			.state_accessor
			.room_state_get_content::<RoomPowerLevelsEventContent>(
				room_id,
				&StateEventType::RoomPowerLevels,
				"",
			)
			.await
			.ok();

		let user_can_demote_self =
			room_power_levels
				.as_ref()
				.is_some_and(|power_levels_content| {
					RoomPowerLevels::from(power_levels_content.clone())
						.user_can_change_user_power_level(user_id, user_id)
				}) || services
				.rooms
				.state_accessor
				.room_state_get(room_id, &StateEventType::RoomCreate, "")
				.await
				.is_ok_and(|event| event.sender() == user_id);

		if user_can_demote_self {
			let mut power_levels_content = room_power_levels.unwrap_or_default();
			power_levels_content.users.remove(user_id);

			// ignore errors so deactivation doesn't fail
			match services
				.rooms
				.timeline
				.build_and_append_pdu(
					PduBuilder::state(String::new(), &power_levels_content),
					user_id,
					room_id,
					&state_lock,
				)
				.await
			{
				| Err(e) => {
					warn!(%room_id, %user_id, "Failed to demote user's own power level: {e}");
				},
				| _ => {
					info!("Demoted {user_id} in {room_id} as part of account deactivation");
				},
			}
		}
	}

	super::leave_all_rooms(services, user_id)
		.boxed()
		.await;

	Ok(())
}
