use std::{mem, ops::Deref};

use async_trait::async_trait;
use axum::{body::Body, extract::FromRequest};
use bytes::{BufMut, Bytes, BytesMut};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, DeviceId, OwnedDeviceId, OwnedServerName,
	OwnedUserId, ServerName, UserId, api::IncomingRequest,
};
use tuwunel_core::{Error, Result, debug, debug_warn, err, trace, utils::string::EMPTY};
use tuwunel_service::{Services, appservice::RegistrationInfo};

use super::{auth, auth::Auth, request, request::Request};
use crate::State;

/// Extractor for Ruma request structs
pub(crate) struct Args<T> {
	/// Request struct body
	pub(crate) body: T,

	/// Federation server authentication: X-Matrix origin
	/// None when not a federation server.
	pub(crate) origin: Option<OwnedServerName>,

	/// Local user authentication: user_id.
	/// None when not an authenticated local user.
	pub(crate) sender_user: Option<OwnedUserId>,

	/// Local user authentication: device_id.
	/// None when not an authenticated local user or no device.
	pub(crate) sender_device: Option<OwnedDeviceId>,

	/// Appservice authentication; registration info.
	/// None when not an appservice.
	pub(crate) appservice_info: Option<RegistrationInfo>,

	/// Parsed JSON content.
	/// None when body is not a valid string
	pub(crate) json_body: Option<CanonicalJsonValue>,
}

impl<T> Args<T>
where
	T: IncomingRequest + Send + Sync + 'static,
{
	#[inline]
	pub(crate) fn sender(&self) -> (&UserId, &DeviceId) {
		(self.sender_user(), self.sender_device())
	}

	#[inline]
	pub(crate) fn sender_user(&self) -> &UserId {
		self.sender_user
			.as_deref()
			.expect("user must be authenticated for this handler")
	}

	#[inline]
	pub(crate) fn sender_device(&self) -> &DeviceId {
		self.sender_device
			.as_deref()
			.expect("user must be authenticated and device identified")
	}

	#[inline]
	pub(crate) fn origin(&self) -> &ServerName {
		self.origin
			.as_deref()
			.expect("server must be authenticated for this handler")
	}
}

impl<T> Deref for Args<T>
where
	T: IncomingRequest + Send + Sync + 'static,
{
	type Target = T;

	fn deref(&self) -> &Self::Target {
		&self.body
	}
}

#[async_trait]
impl<T> FromRequest<State, Body> for Args<T>
where
	T: IncomingRequest + Send + Sync + 'static,
{
	type Rejection = Error;

	async fn from_request(
		request: hyper::Request<Body>,
		services: &State,
	) -> Result<Self, Self::Rejection> {
		let mut request = request::from(services, request).await?;
		let mut json_body = serde_json::from_slice::<CanonicalJsonValue>(&request.body).ok();

		// while very unusual and really shouldn't be recommended, Synapse accepts POST
		// requests with a completely empty body. very old clients, libraries, and some
		// appservices still call APIs like /join like this. so let's just default to
		// empty object `{}` to copy synapse's behaviour
		if json_body.is_none()
			&& request.parts.method == http::Method::POST
			&& !request.parts.uri.path().contains("/media/")
		{
			trace!("json_body from_request: {:?}", json_body.clone());
			debug_warn!(
				"received a POST request with an empty body, defaulting/assuming to {{}} like \
				 Synapse does"
			);
			json_body = Some(CanonicalJsonValue::Object(CanonicalJsonObject::new()));
		}
		let auth = auth::auth(services, &mut request, json_body.as_ref(), &T::METADATA).await?;
		Ok(Self {
			body: make_body::<T>(services, &mut request, json_body.as_mut(), &auth)?,
			origin: auth.origin,
			sender_user: auth.sender_user,
			sender_device: auth.sender_device,
			appservice_info: auth.appservice_info,
			json_body,
		})
	}
}

fn make_body<T>(
	services: &Services,
	request: &mut Request,
	json_body: Option<&mut CanonicalJsonValue>,
	auth: &Auth,
) -> Result<T>
where
	T: IncomingRequest,
{
	let body = take_body(services, request, json_body, auth);
	let http_request = into_http_request(request, body);
	T::try_from_http_request(http_request, &request.path)
		.map_err(|e| err!(Request(BadJson(debug_warn!("{e}")))))
}

fn into_http_request(request: &Request, body: Bytes) -> hyper::Request<Bytes> {
	let mut http_request = hyper::Request::builder()
		.uri(request.parts.uri.clone())
		.method(request.parts.method.clone());

	*http_request
		.headers_mut()
		.expect("mutable http headers") = request.parts.headers.clone();

	let http_request = http_request
		.body(body)
		.expect("http request body");

	let headers = http_request.headers();
	let method = http_request.method();
	let uri = http_request.uri();
	debug!("{method:?} {uri:?} {headers:?}");

	http_request
}

#[allow(clippy::needless_pass_by_value)]
fn take_body(
	services: &Services,
	request: &mut Request,
	json_body: Option<&mut CanonicalJsonValue>,
	auth: &Auth,
) -> Bytes {
	let Some(CanonicalJsonValue::Object(json_body)) = json_body else {
		return mem::take(&mut request.body);
	};

	let user_id = auth.sender_user.clone().unwrap_or_else(|| {
		let server_name = services.globals.server_name();
		UserId::parse_with_server_name(EMPTY, server_name).expect("valid user_id")
	});

	let uiaa_request = json_body
		.get("auth")
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|auth| auth.get("session"))
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|session| {
			services
				.uiaa
				.get_uiaa_request(&user_id, auth.sender_device.as_deref(), session)
		});

	if let Some(CanonicalJsonValue::Object(initial_request)) = uiaa_request {
		for (key, value) in initial_request {
			json_body.entry(key).or_insert(value);
		}
	}

	let mut buf = BytesMut::new().writer();
	serde_json::to_writer(&mut buf, &json_body).expect("value serialization can't fail");
	buf.into_inner().freeze()
}
