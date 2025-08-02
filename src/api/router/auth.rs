use std::{fmt::Debug, time::SystemTime};

use axum::RequestPartsExt;
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Bearer},
	typed_header::TypedHeaderRejectionReason,
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
	CanonicalJsonObject, CanonicalJsonValue, OwnedDeviceId, OwnedServerName, OwnedUserId, UserId,
	api::{
		AuthScheme, IncomingRequest, Metadata,
		client::{
			directory::get_public_rooms,
			error::ErrorKind,
			profile::{
				get_avatar_url, get_display_name, get_profile, get_profile_key, get_timezone_key,
			},
			voip::get_turn_server_info,
		},
		federation::{authentication::XMatrix, openid::get_openid_userinfo},
	},
};
use tuwunel_core::{
	Err, Error, Result, debug_error, err, is_less_than, utils::result::LogDebugErr, warn,
};
use tuwunel_service::{
	Services,
	appservice::RegistrationInfo,
	server_keys::{PubKeyMap, PubKeys},
};

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
	ret,
	err
)]
pub(super) async fn auth(
	services: &Services,
	request: &mut Request,
	json_body: Option<&CanonicalJsonValue>,
	metadata: &Metadata,
) -> Result<Auth> {
	use AuthScheme::{AccessToken, AccessTokenOptional, AppserviceToken, ServerSignatures};
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

		| (AccessToken, Token::None) => match metadata {
			| &get_turn_server_info::v3::Request::METADATA
				if services.server.config.turn_allow_guests =>
				Ok(Auth::default()),

			| _ => Err!(Request(MissingToken("Missing access token."))),
		},

		| (AppserviceToken, User(_)) =>
			Err!(Request(Unauthorized("Appservice tokens must be used on this endpoint."))),

		| (ServerSignatures, Appservice(_) | User(_)) =>
			Err!(Request(Unauthorized("Server signatures must be used on this endpoint."))),

		| (ServerSignatures, Token::None) => Ok(auth_server(services, request, json_body).await?),

		| (AccessToken, Appservice(info)) => Ok(auth_appservice(services, request, info).await?),

		| (AccessToken | AccessTokenOptional | AuthScheme::None, User(user)) => Ok(Auth {
			sender_user: Some(user.0),
			sender_device: Some(user.1),
			_expires_at: user.2,
			..Auth::default()
		}),

		| (AuthScheme::None | AccessTokenOptional | AppserviceToken, Appservice(info)) =>
			Ok(Auth {
				appservice_info: Some(*info),
				..Auth::default()
			}),

		| (AuthScheme::None | AccessTokenOptional | AppserviceToken, Token::None) =>
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
		| &get_profile_key::unstable::Request::METADATA
		| &get_display_name::v3::Request::METADATA
		| &get_avatar_url::v3::Request::METADATA
		| &get_timezone_key::unstable::Request::METADATA
			if services
				.server
				.config
				.require_auth_for_profile_requests =>
			match token {
				| Token::Appservice(_) | Token::User(_) => Ok(()),
				| Token::None | Token::Expired(_) | Token::Invalid =>
					Err!(Request(MissingToken("Missing or invalid access token."))),
			},
		| &get_public_rooms::v3::Request::METADATA
			if !services
				.server
				.config
				.allow_public_room_directory_without_auth =>
			match token {
				| Token::Appservice(_) | Token::User(_) => Ok(()),
				| Token::None | Token::Expired(_) | Token::Invalid =>
					Err!(Request(MissingToken("Missing or invalid access token."))),
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

async fn auth_appservice(
	services: &Services,
	request: &Request,
	info: Box<RegistrationInfo>,
) -> Result<Auth> {
	let user_id_default = || {
		UserId::parse_with_server_name(
			info.registration.sender_localpart.as_str(),
			services.globals.server_name(),
		)
	};

	let Ok(user_id) = request
		.query
		.user_id
		.clone()
		.map_or_else(user_id_default, OwnedUserId::parse)
	else {
		return Err!(Request(InvalidUsername("Username is invalid.")));
	};

	if !info.is_user_match(&user_id) {
		return Err!(Request(Exclusive("User is not in namespace.")));
	}

	Ok(Auth {
		sender_user: Some(user_id),
		appservice_info: Some(*info),
		..Auth::default()
	})
}

async fn auth_server(
	services: &Services,
	request: &mut Request,
	body: Option<&CanonicalJsonValue>,
) -> Result<Auth> {
	type Member = (String, CanonicalJsonValue);
	type Object = CanonicalJsonObject;
	type Value = CanonicalJsonValue;

	let x_matrix = parse_x_matrix(request).await?;
	auth_server_checks(services, &x_matrix)?;

	let destination = services.globals.server_name();
	let origin = &x_matrix.origin;
	let signature_uri = request
		.parts
		.uri
		.path_and_query()
		.expect("all requests have a path")
		.to_string();

	let signature: [Member; 1] =
		[(x_matrix.key.as_str().into(), Value::String(x_matrix.sig.to_string()))];

	let signatures: [Member; 1] = [(origin.as_str().into(), Value::Object(signature.into()))];

	let authorization: Object = if let Some(body) = body.cloned() {
		let authorization: [Member; 6] = [
			("content".into(), body),
			("destination".into(), Value::String(destination.into())),
			("method".into(), Value::String(request.parts.method.as_str().into())),
			("origin".into(), Value::String(origin.as_str().into())),
			("signatures".into(), Value::Object(signatures.into())),
			("uri".into(), Value::String(signature_uri)),
		];

		authorization.into()
	} else {
		let authorization: [Member; 5] = [
			("destination".into(), Value::String(destination.into())),
			("method".into(), Value::String(request.parts.method.as_str().into())),
			("origin".into(), Value::String(origin.as_str().into())),
			("signatures".into(), Value::Object(signatures.into())),
			("uri".into(), Value::String(signature_uri)),
		];

		authorization.into()
	};

	let key = services
		.server_keys
		.get_verify_key(origin, &x_matrix.key)
		.await
		.map_err(|e| err!(Request(Forbidden(warn!("Failed to fetch signing keys: {e}")))))?;

	let keys: PubKeys = [(x_matrix.key.to_string(), key.key)].into();
	let keys: PubKeyMap = [(origin.as_str().into(), keys)].into();
	if let Err(e) = ruma::signatures::verify_json(&keys, authorization) {
		debug_error!("Failed to verify federation request from {origin}: {e}");
		if request.parts.uri.to_string().contains('@') {
			warn!(
				"Request uri contained '@' character. Make sure your reverse proxy gives \
				 tuwunel the raw uri (apache: use nocanon)"
			);
		}

		return Err!(Request(Forbidden("Failed to verify X-Matrix signatures.")));
	}

	Ok(Auth {
		origin: origin.to_owned().into(),
		..Auth::default()
	})
}

fn auth_server_checks(services: &Services, x_matrix: &XMatrix) -> Result {
	if !services.server.config.allow_federation {
		return Err!(Config("allow_federation", "Federation is disabled."));
	}

	let destination = services.globals.server_name();
	if x_matrix.destination.as_deref() != Some(destination) {
		return Err!(Request(Forbidden("Invalid destination.")));
	}

	let origin = &x_matrix.origin;
	if services
		.config
		.forbidden_remote_server_names
		.is_match(origin.host())
	{
		return Err!(Request(Forbidden(debug_warn!(
			"Federation requests from {origin} denied."
		))));
	}

	Ok(())
}

async fn parse_x_matrix(request: &mut Request) -> Result<XMatrix> {
	let TypedHeader(Authorization(x_matrix)) = request
		.parts
		.extract::<TypedHeader<Authorization<XMatrix>>>()
		.await
		.map_err(|e| {
			let msg = match e.reason() {
				| TypedHeaderRejectionReason::Missing => "Missing Authorization header.",
				| TypedHeaderRejectionReason::Error(_) => "Invalid X-Matrix signatures.",
				| _ => "Unknown header-related error",
			};

			err!(Request(Forbidden(warn!("{msg}: {e}"))))
		})?;

	Ok(x_matrix)
}
