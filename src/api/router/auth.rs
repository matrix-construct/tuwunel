use axum::RequestPartsExt;
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Bearer},
	typed_header::TypedHeaderRejectionReason,
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
use tuwunel_core::{Err, Error, Result, debug_error, err, warn};
use tuwunel_service::{
	Services,
	appservice::RegistrationInfo,
	server_keys::{PubKeyMap, PubKeys},
};

use super::request::Request;

enum Token {
	Appservice(Box<RegistrationInfo>),
	User((OwnedUserId, OwnedDeviceId)),
	Invalid,
	None,
}

pub(super) struct Auth {
	pub(super) origin: Option<OwnedServerName>,
	pub(super) sender_user: Option<OwnedUserId>,
	pub(super) sender_device: Option<OwnedDeviceId>,
	pub(super) appservice_info: Option<RegistrationInfo>,
}

pub(super) async fn auth(
	services: &Services,
	request: &mut Request,
	json_body: Option<&CanonicalJsonValue>,
	metadata: &Metadata,
) -> Result<Auth> {
	let bearer: Option<TypedHeader<Authorization<Bearer>>> = request.parts.extract().await?;
	let token = match &bearer {
		| Some(TypedHeader(Authorization(bearer))) => Some(bearer.token()),
		| None => request.query.access_token.as_deref(),
	};

	let token = if let Some(token) = token {
		match services.appservice.find_from_token(token).await {
			| Some(reg_info) => Token::Appservice(Box::new(reg_info)),
			| _ => match services.users.find_from_token(token).await {
				| Ok((user_id, device_id)) => Token::User((user_id, device_id)),
				| _ => Token::Invalid,
			},
		}
	} else {
		Token::None
	};

	if metadata.authentication == AuthScheme::None {
		match metadata {
			| &get_public_rooms::v3::Request::METADATA => {
				if !services
					.server
					.config
					.allow_public_room_directory_without_auth
				{
					match token {
						| Token::Appservice(_) | Token::User(_) => {
							// we should have validated the token above
							// already
						},
						| Token::None | Token::Invalid => {
							return Err(Error::BadRequest(
								ErrorKind::MissingToken,
								"Missing or invalid access token.",
							));
						},
					}
				}
			},
			| &get_profile::v3::Request::METADATA
			| &get_profile_key::unstable::Request::METADATA
			| &get_display_name::v3::Request::METADATA
			| &get_avatar_url::v3::Request::METADATA
			| &get_timezone_key::unstable::Request::METADATA => {
				if services
					.server
					.config
					.require_auth_for_profile_requests
				{
					match token {
						| Token::Appservice(_) | Token::User(_) => {
							// we should have validated the token above
							// already
						},
						| Token::None | Token::Invalid => {
							return Err(Error::BadRequest(
								ErrorKind::MissingToken,
								"Missing or invalid access token.",
							));
						},
					}
				}
			},
			| _ => {},
		}
	}

	match (metadata.authentication, token) {
		| (AuthScheme::AccessToken, Token::Appservice(info)) => {
			Ok(auth_appservice(services, request, info).await?)
		},
		| (
			AuthScheme::None | AuthScheme::AccessTokenOptional | AuthScheme::AppserviceToken,
			Token::Appservice(info),
		) => Ok(Auth {
			origin: None,
			sender_user: None,
			sender_device: None,
			appservice_info: Some(*info),
		}),
		| (AuthScheme::AccessToken, Token::None) => match metadata {
			| &get_turn_server_info::v3::Request::METADATA => {
				if services.server.config.turn_allow_guests {
					Ok(Auth {
						origin: None,
						sender_user: None,
						sender_device: None,
						appservice_info: None,
					})
				} else {
					Err(Error::BadRequest(ErrorKind::MissingToken, "Missing access token."))
				}
			},
			| _ => Err(Error::BadRequest(ErrorKind::MissingToken, "Missing access token.")),
		},
		| (
			AuthScheme::AccessToken | AuthScheme::AccessTokenOptional | AuthScheme::None,
			Token::User((user_id, device_id)),
		) => Ok(Auth {
			origin: None,
			sender_user: Some(user_id),
			sender_device: Some(device_id),
			appservice_info: None,
		}),
		| (AuthScheme::ServerSignatures, Token::None) => {
			Ok(auth_server(services, request, json_body).await?)
		},
		| (
			AuthScheme::None | AuthScheme::AppserviceToken | AuthScheme::AccessTokenOptional,
			Token::None,
		) => Ok(Auth {
			sender_user: None,
			sender_device: None,
			origin: None,
			appservice_info: None,
		}),
		| (AuthScheme::ServerSignatures, Token::Appservice(_) | Token::User(_)) => {
			Err(Error::BadRequest(
				ErrorKind::Unauthorized,
				"Only server signatures should be used on this endpoint.",
			))
		},
		| (AuthScheme::AppserviceToken, Token::User(_)) => Err(Error::BadRequest(
			ErrorKind::Unauthorized,
			"Only appservice access tokens should be used on this endpoint.",
		)),
		| (AuthScheme::None, Token::Invalid) => {
			// OpenID federation endpoint uses a query param with the same name, drop this
			// once query params for user auth are removed from the spec. This is
			// required to make integration manager work.
			if request.query.access_token.is_some()
				&& metadata == &get_openid_userinfo::v1::Request::METADATA
			{
				Ok(Auth {
					origin: None,
					sender_user: None,
					sender_device: None,
					appservice_info: None,
				})
			} else {
				Err(Error::BadRequest(
					ErrorKind::UnknownToken { soft_logout: false },
					"Unknown access token.",
				))
			}
		},
		| (_, Token::Invalid) => Err(Error::BadRequest(
			ErrorKind::UnknownToken { soft_logout: false },
			"Unknown access token.",
		)),
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
		origin: None,
		sender_user: Some(user_id),
		sender_device: None,
		appservice_info: Some(*info),
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
				 conduwuit the raw uri (apache: use nocanon)"
			);
		}

		return Err!(Request(Forbidden("Failed to verify X-Matrix signatures.")));
	}

	Ok(Auth {
		origin: origin.to_owned().into(),
		sender_user: None,
		sender_device: None,
		appservice_info: None,
	})
}

fn auth_server_checks(services: &Services, x_matrix: &XMatrix) -> Result<()> {
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
