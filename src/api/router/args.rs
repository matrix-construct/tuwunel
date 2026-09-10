use std::{any::TypeId, fmt::Debug, ops::Deref};

use axum::{body::Body, extract::FromRequest};
use axum_extra::extract::cookie::CookieJar;
use bytes::{BufMut, Bytes, BytesMut};
use http::{Method, Request as HttpRequest};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, DeviceId, OwnedDeviceId, OwnedServerName,
	OwnedUserId, ServerName, UserId, api::IncomingRequest,
};
use serde_json::{Value as JsonValue, from_slice};
use tuwunel_core::{Error, Result, err, implement, utils::string::EMPTY};
use tuwunel_service::{Services, appservice::RegistrationInfo};

use super::{
	auth::{Auth, AuthDispatch, auth},
	request::{Request, from as request_from},
};
use crate::{State, client::admin::require_admin};

/// Extracts a typed Ruma request and its authentication context.
///
/// Administrator-only routes defer JSON errors until authorization succeeds.
/// Ordinary routes reject malformed JSON before authentication.
#[derive(Debug)]
pub(crate) struct Args<T, const ADMIN: bool = false> {
	/// Request struct body
	pub(crate) body: T,

	/// Cookies received from the useragent.
	pub(crate) cookie: CookieJar,

	/// Authenticated X-Matrix origin, absent for non-federation requests.
	pub(crate) origin: Option<OwnedServerName>,

	/// Authenticated local user, absent when no local user is identified.
	pub(crate) sender_user: Option<OwnedUserId>,

	/// Authenticated local device, absent for device-less authentication.
	pub(crate) sender_device: Option<OwnedDeviceId>,

	/// Authenticated appservice registration, absent for other callers.
	pub(crate) appservice_info: Option<RegistrationInfo>,

	/// Parsed canonical JSON, absent for raw or noncanonical request bodies.
	pub(crate) json_body: Option<CanonicalJsonValue>,
}

/// Requires administrator authorization before returning request body errors.
///
/// Routes opt in through this alias; the default extractor retains its ordering.
pub(crate) type ArgsAdmin<T> = Args<T, true>;

/// Returns the user authenticated for a route requiring a user identity.
///
/// Panics if the endpoint's authentication scheme did not identify a user.
#[implement(
	Args,
	generics = "<T, const ADMIN: bool>",
	params = "<T, ADMIN>"
)]
#[inline]
pub(crate) fn sender_user(&self) -> &UserId {
	self.sender_user
		.as_deref()
		.expect("user must be authenticated for this handler")
}

/// Returns the server authenticated for a federation route.
///
/// Panics if the endpoint's authentication scheme did not identify a server.
#[implement(
	Args,
	generics = "<T, const ADMIN: bool>",
	params = "<T, ADMIN>"
)]
#[inline]
pub(crate) fn origin(&self) -> &ServerName {
	self.origin
		.as_deref()
		.expect("server must be authenticated for this handler")
}

/// Returns the authenticated device or rejects a device-less request.
///
/// User authentication alone does not guarantee a device identity.
#[implement(
	Args,
	generics = "<T, const ADMIN: bool>",
	params = "<T, ADMIN>"
)]
#[inline]
pub(crate) fn sender_device(&self) -> Result<&DeviceId> {
	self.sender_device
		.as_deref()
		.ok_or(err!(Request(Forbidden("user must be authenticated and device identified"))))
}

impl<T, const ADMIN: bool> Deref for Args<T, ADMIN>
where
	T: Sync,
{
	type Target = T;

	fn deref(&self) -> &Self::Target { &self.body }
}

impl<T, const ADMIN: bool> FromRequest<State, Body> for Args<T, ADMIN>
where
	T: IncomingRequest + Debug + Send + Sync + 'static,
	T::Authentication: AuthDispatch,
{
	type Rejection = Error;

	#[tracing::instrument(name = "ar", level = "debug", skip_all, err(level = "debug"))]
	async fn from_request(
		request: HttpRequest<Body>,
		services: &State,
	) -> Result<Self, Self::Rejection> {
		let request = request_from(services, request).await?;
		let json_body = match ADMIN {
			| true => parse_json(&request),
			| false => Ok(parse_json(&request)?),
		};

		let json = json_body.as_ref().ok().and_then(Option::as_ref);
		let (request, auth) = authenticate::<T>(request, services, json).await?;

		_ = request
			.parts
			.extensions
			.get::<tracing::Span>()
			.inspect(|span| record_auth_context(span, &auth));

		if ADMIN {
			let sender = auth.sender_user.as_deref().ok_or_else(|| {
				err!(Request(Forbidden("Only server administrators can use this endpoint")))
			})?;

			require_admin(services, sender).await?;
		}

		make_args(services, request, json_body?, auth)
	}
}

fn record_auth_context(span: &tracing::Span, auth: &Auth) {
	_ = auth
		.sender_user
		.as_deref()
		.inspect(|sender_user| {
			span.record("user_id", sender_user.as_str());
		});

	_ = auth
		.sender_device
		.as_deref()
		.inspect(|sender_device| {
			span.record("device_id", sender_device.as_str());
		});

	_ = auth.origin.as_deref().inspect(|origin| {
		span.record("origin", origin.as_str());
	});
}

/// Parses canonical JSON while retaining ordinary JSON for typed deserialization.
///
/// Empty POST and DELETE bodies become objects for UIA. Other methods and media
/// uploads preserve their existing body handling.
fn parse_json(request: &Request) -> Result<Option<CanonicalJsonValue>> {
	let json_body = from_slice(&request.body).ok();
	let json_endpoint = matches!(
		request.parts.method,
		Method::POST | Method::PUT | Method::DELETE | Method::PATCH
	) && !request.parts.uri.path().contains("/media/");

	if json_body.is_some() || !json_endpoint {
		return Ok(json_body);
	}

	let empty = request.body.iter().all(u8::is_ascii_whitespace);

	if !empty {
		from_slice::<JsonValue>(&request.body)
			.map_err(|_| err!(Request(NotJson("Request body is not valid JSON."))))?;
	}

	let empty_object = (empty && matches!(request.parts.method, Method::POST | Method::DELETE))
		.then(|| CanonicalJsonValue::Object(CanonicalJsonObject::new()));

	Ok(empty_object)
}

/// Authenticates a request while retaining the headers consumed by extraction.
///
/// The parsed canonical body remains available to federation authentication.
async fn authenticate<T>(
	mut request: Request,
	services: &State,
	json_body: Option<&CanonicalJsonValue>,
) -> Result<(Request, Auth)>
where
	T: IncomingRequest + Debug + Send + Sync + 'static,
	T::Authentication: AuthDispatch,
{
	let auth =
		auth::<T::Authentication>(services, &mut request, json_body, TypeId::of::<T>()).await?;

	Ok((request, auth))
}

/// Builds typed arguments after authentication and any UIA body merge.
///
/// Transfers the original HTTP parts without cloning headers or the URI.
fn make_args<T, const ADMIN: bool>(
	services: &Services,
	request: Request,
	json_body: Option<CanonicalJsonValue>,
	auth: Auth,
) -> Result<Args<T, ADMIN>>
where
	T: IncomingRequest,
{
	let json_body = json_body.map(|json| match json {
		| CanonicalJsonValue::Object(json) => restore_body(services, json, &auth).into(),
		| json => json,
	});

	let body = json_body
		.as_ref()
		.filter(|json| json.is_object())
		.map_or(request.body, serialize_body);

	let http_request = HttpRequest::from_parts(request.parts, body);
	let body = T::try_from_http_request(http_request, &request.path)
		.map_err(|e| err!(Request(BadJson(debug_warn!("{e}")))))?;

	Ok(Args {
		body,
		cookie: request.cookie,
		origin: auth.origin,
		sender_user: auth.sender_user,
		sender_device: auth.sender_device,
		appservice_info: auth.appservice_info,
		json_body,
	})
}

/// Restores omitted fields from a UIA session before typed body parsing.
///
/// Current request fields take precedence over the saved initial request.
fn restore_body(
	services: &Services,
	json_body: CanonicalJsonObject,
	auth: &Auth,
) -> CanonicalJsonObject {
	let uiaa_request = json_body
		.get("auth")
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|auth| auth.get("session"))
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|session| {
			let user_id = auth.sender_user.clone().unwrap_or_else(|| {
				UserId::parse_with_server_name(EMPTY, services.globals.server_name())
					.expect("valid user_id")
			});

			services
				.uiaa
				.get_uiaa_request(&user_id, auth.sender_device.as_deref(), session)
		});

	uiaa_request
		.and_then(|json| match json {
			| CanonicalJsonValue::Object(json) => Some(json),
			| _ => None,
		})
		.into_iter()
		.flatten()
		.fold(json_body, |mut json, (key, value)| {
			json.entry(key).or_insert(value);

			json
		})
}

/// Serializes a canonical body directly into the HTTP byte buffer.
///
/// Canonical JSON values are always serializable, including restored UIA bodies.
fn serialize_body(json_body: &CanonicalJsonValue) -> Bytes {
	let mut buf = BytesMut::new().writer();

	serde_json::to_writer(&mut buf, json_body).expect("value serialization can't fail");

	buf.into_inner().freeze()
}
