mod appservice;
mod dispatch;
mod server;
mod uiaa;

use std::{any::TypeId, fmt::Debug, time::SystemTime};

use axum::RequestPartsExt;
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Bearer},
};
use futures::{
	TryFutureExt,
	future::{
		Either::{Left, Right},
		select_ok, try_join,
	},
	pin_mut,
};
use ruma::{
	CanonicalJsonValue, OwnedDeviceId, OwnedServerName, OwnedUserId,
	api::client::{
		directory::get_public_rooms,
		profile::{
			delete_profile_field, get_avatar_url, get_display_name, get_profile,
			get_profile_field, set_avatar_url, set_display_name, set_profile_field,
		},
		session::{logout, logout_all},
	},
};
use tuwunel_core::{Err, Result, is_less_than, smallstr::SmallString};
use tuwunel_service::{Services, appservice::RegistrationInfo};

pub(super) use self::dispatch::AuthDispatch;
use self::dispatch::Scheme;
pub(crate) use self::uiaa::auth_uiaa;
use super::request::Request;

type AccessToken = SmallString<[u8; 32]>;

pub(super) enum Token {
	Appservice(Box<RegistrationInfo>),
	User((OwnedUserId, OwnedDeviceId, Option<SystemTime>)),
	Expired(AccessToken),
	Invalid,
	None,
}

#[derive(Debug, Default)]
pub(super) struct Auth {
	pub(super) origin: Option<OwnedServerName>,
	pub(super) sender_user: Option<OwnedUserId>,
	pub(super) sender_device: Option<OwnedDeviceId>,
	pub(super) appservice_info: Option<RegistrationInfo>,
	pub(super) _expires_at: Option<SystemTime>,
}

#[tracing::instrument(
	level = "trace",
	skip(services, request, json_body),
	err(level = "debug"),
	ret
)]
pub(super) async fn auth<A: AuthDispatch>(
	services: &Services,
	request: &mut Request,
	json_body: Option<&CanonicalJsonValue>,
	route: TypeId,
) -> Result<Auth> {
	let bearer: Option<TypedHeader<Authorization<Bearer>>> =
		request.parts.extract().await.unwrap_or(None);

	let access_token = match &bearer {
		| Some(TypedHeader(Authorization(bearer))) => Some(bearer.token()),
		| None => request.query.access_token.as_deref(),
	};

	let token = match find_token(services, access_token).await? {
		| Token::User((_, _, expires_at))
			if expires_at.is_some_and(is_less_than!(SystemTime::now())) =>
			Token::Expired(access_token.unwrap_or_default().into()),

		| token => token,
	};

	if A::SCHEME == Scheme::None {
		check_auth_still_required(services, &token, route)?;
	}

	let auth = A::dispatch(services, request, json_body, token, route).await?;

	try_join(
		locked_account_check(services, &auth, route),
		suspended_account_check(services, &auth, route),
	)
	.await?;

	Ok(auth)
}

/// MSC3939: 401 `M_USER_LOCKED` for locked accounts; logout endpoints
/// bypass. `soft_logout: true` is emitted by ruma for this errcode.
#[inline(never)]
async fn locked_account_check(services: &Services, auth: &Auth, route: TypeId) -> Result {
	let Some(user_id) = auth.sender_user.as_deref() else {
		return Ok(());
	};

	let is_logout = route == TypeId::of::<logout::v3::Request>()
		|| route == TypeId::of::<logout_all::v3::Request>();

	if is_logout || !services.users.is_locked(user_id).await {
		return Ok(());
	}

	Err!(Request(UserLocked("This account has been locked.")))
}

/// MSC3823: 403 `M_USER_SUSPENDED` on `set_display_name` / `set_avatar_url`
/// for suspended callers. Companion checks: per-field in the profile
/// handlers, per-PDU in `timeline::build_and_append_pdu`.
#[inline(never)]
async fn suspended_account_check(services: &Services, auth: &Auth, route: TypeId) -> Result {
	let Some(user_id) = auth.sender_user.as_deref() else {
		return Ok(());
	};

	let blocked = route == TypeId::of::<set_display_name::v3::Request>()
		|| route == TypeId::of::<set_avatar_url::v3::Request>()
		|| route == TypeId::of::<set_profile_field::v3::Request>()
		|| route == TypeId::of::<delete_profile_field::v3::Request>();

	if !blocked || !services.users.is_suspended(user_id).await {
		return Ok(());
	}

	Err!(Request(UserSuspended("Account is suspended.")))
}

#[inline(never)]
fn check_auth_still_required(services: &Services, token: &Token, route: TypeId) -> Result {
	let is_profile = route == TypeId::of::<get_profile::v3::Request>()
		|| route == TypeId::of::<get_profile_field::v3::Request>()
		|| route == TypeId::of::<get_display_name::v3::Request>()
		|| route == TypeId::of::<get_avatar_url::v3::Request>();

	let is_public_rooms = route == TypeId::of::<get_public_rooms::v3::Request>();

	if (is_profile
		&& services
			.server
			.config
			.require_auth_for_profile_requests)
		|| (is_public_rooms
			&& !services
				.server
				.config
				.allow_public_room_directory_without_auth)
	{
		match token {
			| Token::Appservice(_) | Token::User(_) => Ok(()),
			| Token::None | Token::Expired(_) | Token::Invalid =>
				Err!(Request(MissingToken("Missing or invalid access token."))),
		}
	} else {
		Ok(())
	}
}

async fn find_token(services: &Services, token: Option<&str>) -> Result<Token> {
	let Some(token) = token else {
		return Ok(Token::None);
	};

	let user_token = services
		.users
		.find_from_token(token)
		.map_ok(Token::User);

	let appservice_token = services
		.appservice
		.find_from_access_token(token)
		.map_ok(Box::new)
		.map_ok(Token::Appservice);

	pin_mut!(user_token, appservice_token);
	match select_ok([Left(user_token), Right(appservice_token)]).await {
		| Err(e) if !e.is_not_found() => Err(e),
		| Ok((token, _)) => Ok(token),
		| _ => Ok(Token::Invalid),
	}
}
