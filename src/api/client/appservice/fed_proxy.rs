use axum::extract::State;
use http::Method;
use ruma::{
	CanonicalJsonObject, ServerName,
	api::client::appservice::fed_proxy::unstable::{Request, Response},
	serde::{JsonObject, Raw},
};
use serde_json::{from_slice, from_str, to_vec};
use tuwunel_core::{
	Err, Result, err,
	utils::url::{is_plain_path, with_query},
};
use tuwunel_service::{
	Services,
	appservice::{
		RegistrationInfo,
		proxy::{Root, split_path},
	},
};

use crate::Ruma;

/// # `POST /_matrix/client/unstable/io.element.msc4512/appservice/fed_proxy`
///
/// Signs and sends a federation request for an appservice (MSC4512).
///
/// The path must lie under the appservice's proxy prefix and the destination
/// be a remote server this one federates with. The remote's status and JSON
/// body are relayed whatever the status, so the appservice reads a remote
/// refusal as it would its own.
pub(crate) async fn fed_proxy_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let appservice = body.appservice_info.as_ref().ok_or_else(|| {
		err!(Request(Forbidden("This endpoint can only be called by appservices.")))
	})?;

	let Request {
		destination,
		method,
		path,
		query,
		body: json,
	} = &body.body;

	let method = proxied_method(method, json.is_some())?;
	let json = json.as_ref().map(canonical_object).transpose()?;

	if !path_allowed(appservice, path) {
		return Err!(Request(UnstableFedProxyPathNotAllowed(
			"Path is outside the appservice's proxy prefix."
		)));
	}

	if !destination_allowed(&services, destination) {
		return Err!(Request(UnstableFedProxyDestinationDenied(
			"Destination is not a server this one federates with."
		)));
	}

	let path_and_query = with_query(path, query.iter());

	let (status, content) = services
		.federation
		.execute_raw(destination, method, &path_and_query, json)
		.await
		.map_err(|e| {
			err!(Request(UnstableFedProxyConnectionFailed(debug_warn!(
				%destination,
				error = %e,
				"Could not deliver the proxied request"
			))))
		})?;

	Ok(Response {
		status: status.as_u16().into(),
		content: json_object(&content),
	})
}

/// The request's method, if the MSC allows it with or without a body as given.
///
/// The MSC allows GET, POST, PUT and DELETE, and a body with POST and PUT
/// only.
fn proxied_method(method: &str, has_body: bool) -> Result<Method> {
	let (method, takes_body) = match method {
		| "GET" => (Method::GET, false),
		| "DELETE" => (Method::DELETE, false),
		| "POST" => (Method::POST, true),
		| "PUT" => (Method::PUT, true),
		| _ => return Err!(Request(InvalidParam("Method must be GET, POST, PUT or DELETE."))),
	};

	if has_body && !takes_body {
		return Err!(Request(InvalidParam("A {method} request takes no body.")));
	}

	Ok(method)
}

/// The request body as the canonical JSON it is signed and sent as.
///
/// The MSC requires an object; the request's other fields can leave the
/// body's text as the appservice wrote it, so it is parsed and written out
/// again rather than sent as received.
fn canonical_object(json: &Raw<JsonObject>) -> Result<Vec<u8>> {
	let object: CanonicalJsonObject = from_str(json.json().get())
		.map_err(|e| err!(Request(InvalidParam("Body must be a canonical JSON object: {e}"))))?;

	Ok(to_vec(&object)?)
}

/// Whether a federation path lies under the appservice's proxy prefix.
///
/// It must be a plain path, versioned as the proxied paths are, and hold no
/// dot segment even once percent-decoded.
fn path_allowed(appservice: &RegistrationInfo, path: &str) -> bool {
	is_plain_path(path)
		&& split_path(path).is_some_and(|(root, remainder)| {
			root == Root::Federation && appservice.proxy_covers(remainder)
		})
}

/// Whether a request may go to `destination`.
///
/// Only a remote server this one federates with qualifies; its own name
/// never does.
fn destination_allowed(services: &Services, destination: &ServerName) -> bool {
	destination != services.globals.server_name()
		&& services.config.is_remote_permitted(destination)
}

/// The remote body, if it is a JSON object.
///
/// The MSC relays `content` as an object, so any other body, JSON or not, is
/// left out.
fn json_object(body: &[u8]) -> Option<Raw<JsonObject>> {
	from_slice(body)
		.ok()
		.filter(|json: &Raw<JsonObject>| json.json().get().starts_with('{'))
}
