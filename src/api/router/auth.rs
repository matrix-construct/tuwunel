mod appservice;
mod server;
mod uiaa;

use std::{fmt::Debug, time::SystemTime};

use axum::RequestPartsExt;
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Bearer},
};
use futures::{
	TryFutureExt,
	future::{
		Either::{Left, Right},
		select_ok,
	},
	pin_mut,
};
use ruma::{
	CanonicalJsonValue, OwnedDeviceId, OwnedServerName, OwnedUserId,
	api::{
		AuthScheme, IncomingRequest, Metadata,
		client::{
			directory::get_public_rooms,
			error::ErrorKind,
			profile::{
				get_avatar_url, get_display_name, get_profile, get_profile_field,
				get_timezone_key,
			},
			voip::get_turn_server_info,
		},
		federation::openid::get_openid_userinfo,
	},
};
use tuwunel_core::{Err, Error, Result, is_less_than, utils::result::LogDebugErr};
use tuwunel_service::{Services, appservice::RegistrationInfo};

pub(crate) use self::uiaa::auth_uiaa;
use self::{appservice::auth_appservice, server::auth_server};
use super::request::Request;

enum Token {
	Appservice(Box<RegistrationInfo>),
	User((OwnedUserId, OwnedDeviceId, Option<SystemTime>)),
	Expired((OwnedUserId, OwnedDeviceId)),
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
pub(super) async fn auth(
	services: &Services,
	request: &mut Request,
	json_body: Option<&CanonicalJsonValue>,
	metadata: &Metadata,
) -> Result<Auth> {
	use AuthScheme::{
		AccessToken, AccessTokenOptional, AppserviceToken, AppserviceTokenOptional,
		ServerSignatures,
	};
	use Error::BadRequest;
	use ErrorKind::UnknownToken;
	use Token::{Appservice, Expired, Invalid, User};

	let bearer: Option<TypedHeader<Authorization<Bearer>>> =
		request.parts.extract().await.unwrap_or(None);

	let token = match &bearer {
		| Some(TypedHeader(Authorization(bearer))) => Some(bearer.token()),
		| None => request.query.access_token.as_deref(),
	};

	let token = match find_token(services, token).await? {
		| User((user_id, device_id, expires_at))
			if expires_at.is_some_and(is_less_than!(SystemTime::now())) =>
			Expired((user_id, device_id)),

		| token => token,
	};

	if metadata.authentication == AuthScheme::None {
		check_auth_still_required(services, metadata, &token)?;
	}

	match (metadata.authentication, token) {
		| (AuthScheme::None, Invalid)
			if request.query.access_token.is_some()
				&& metadata == &get_openid_userinfo::v1::Request::METADATA =>
		{
			// OpenID federation endpoint uses a query param with the same name, drop this
			// once query params for user auth are removed from the spec. This is
			// required to make integration manager work.
			Ok(Auth::default())
		},

		| (_, Invalid) =>
			Err(BadRequest(UnknownToken { soft_logout: false }, "Unknown access token.")),

		| (_, Expired((user_id, device_id))) => {
			services
				.users
				.remove_access_token(&user_id, &device_id)
				.await
				.log_debug_err()
				.ok();

			Err(BadRequest(UnknownToken { soft_logout: true }, "Expired access token."))
		},

		| (AppserviceToken, User(_)) => {
			Err!(Request(Unauthorized("Appservice tokens must be used on this endpoint.")))
		},

		| (ServerSignatures, Appservice(_) | User(_)) => {
			Err!(Request(Unauthorized("Server signatures must be used on this endpoint.")))
		},

		| (ServerSignatures, Token::None) => Ok(auth_server(services, request, json_body).await?),

		| (AccessToken, Appservice(info)) => Ok(auth_appservice(services, request, info).await?),

		| (AccessToken | AppserviceToken, Token::None) => match metadata {
			| &get_turn_server_info::v3::Request::METADATA
				if services.server.config.turn_allow_guests =>
				Ok(Auth::default()),

			| _ => Err!(Request(MissingToken("Missing access token."))),
		},

		| (
			AccessToken | AccessTokenOptional | AppserviceTokenOptional | AuthScheme::None,
			User(user),
		) => Ok(Auth {
			sender_user: Some(user.0),
			sender_device: Some(user.1),
			_expires_at: user.2,
			..Auth::default()
		}),

		| (
			AccessTokenOptional | AppserviceTokenOptional | AppserviceToken | AuthScheme::None,
			Appservice(info),
		) => Ok(Auth {
			appservice_info: Some(*info),
			..Auth::default()
		}),

		| (AccessTokenOptional | AppserviceTokenOptional | AuthScheme::None, Token::None) =>
			Ok(Auth::default()),
	}
}

fn check_auth_still_required(services: &Services, metadata: &Metadata, token: &Token) -> Result {
	debug_assert_eq!(
		metadata.authentication,
		AuthScheme::None,
		"Expected endpoint to be unauthenticated"
	);

	match metadata {
		| &get_profile::v3::Request::METADATA
		| &get_profile_field::v3::Request::METADATA
		| &get_display_name::v3::Request::METADATA
		| &get_avatar_url::v3::Request::METADATA
		| &get_timezone_key::unstable::Request::METADATA
			if services
				.server
				.config
				.require_auth_for_profile_requests =>
			match token {
				| Token::Appservice(_) | Token::User(_) => Ok(()),
				| Token::None | Token::Expired(_) | Token::Invalid => {
					Err!(Request(MissingToken("Missing or invalid access token.")))
				},
			},
		| &get_public_rooms::v3::Request::METADATA
			if !services
				.server
				.config
				.allow_public_room_directory_without_auth =>
			match token {
				| Token::Appservice(_) | Token::User(_) => Ok(()),
				| Token::None | Token::Expired(_) | Token::Invalid => {
					Err!(Request(MissingToken("Missing or invalid access token.")))
				},
			},
		| _ => Ok(()),
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
