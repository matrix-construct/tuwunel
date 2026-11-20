//! MSC4512: forwarding the paths an appservice claims.
//!
//! A request no route matched is checked against the appservices' claims; a
//! claimed one is authenticated as a native route would be and sent on, and
//! any other stays the plain 404.

use std::any::TypeId;

use axum::{
	RequestExt,
	body::Body,
	extract::Path,
	response::{IntoResponse, Response},
};
use bytes::Bytes;
use http::{
	HeaderMap, HeaderName, Method, Request as HttpRequest, Response as HttpResponse, StatusCode,
	Uri,
	header::{ACCEPT, ACCEPT_LANGUAGE, CONNECTION, CONTENT_TYPE},
};
use itertools::Itertools;
use reqwest::{Body as ReqwestBody, Response as ReqwestResponse};
use ruma::api::{
	auth_scheme::AccessToken, error::ErrorKind, federation::authentication::ServerSignatures,
};
use tuwunel_core::{
	Err, Error, Result, err,
	utils::{BoolExt, url::is_plain_path},
};
use tuwunel_service::{
	Services,
	appservice::{
		RegistrationInfo,
		proxy::{Root, split_path},
	},
};
use url::form_urlencoded::parse as parse_query;

use super::{
	args::{record_auth_context, serialize_body},
	auth::{Auth, auth},
	request::{PathParams, Request, from_parts},
};

/// Marks a proxied request in the authentication checks keyed by route.
///
/// No route-keyed exemption names it, so a locked account is refused and a
/// missing token is too.
struct Proxied;

/// Request headers passed to the appservice.
///
/// Every other header, the caller's credentials and cookies included, stays
/// here.
const FORWARDED_HEADERS: [HeaderName; 3] = [CONTENT_TYPE, ACCEPT, ACCEPT_LANGUAGE];

/// Response headers never relayed from the appservice.
///
/// The hop-by-hop set, then those whose meaning belongs to this server:
/// cookies, and the CORS and framing policy its own layers set.
const DROPPED_HEADERS: [&str; 12] = [
	"connection",
	"keep-alive",
	"proxy-authenticate",
	"proxy-authorization",
	"te",
	"trailer",
	"trailers",
	"transfer-encoding",
	"upgrade",
	"set-cookie",
	"content-security-policy",
	"x-frame-options",
];

const USER_IDENTIFIER: HeaderName = HeaderName::from_static("x-matrix-user-identifier");

const ORIGIN: HeaderName = HeaderName::from_static("x-matrix-origin");

/// Answers a request no route matched.
///
/// A path an appservice claims (MSC4512) is authenticated and forwarded to
/// it, the response streaming back; any other request is 404 before it is
/// authenticated or its body read.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn proxy(services: &Services, request: HttpRequest<Body>) -> Result<Response> {
	let (root, appservice) = claim(services, request.method(), request.uri())
		.await
		.ok_or_else(not_found)?;

	// The forwarding URL is parsed again, which would rewrite such a path.
	if !is_plain_path(request.uri().path()) {
		return Err!(Request(InvalidParam("Proxied path is not a plain path.")));
	}

	let (parts, body) = request.with_limited_body().into_parts();
	let request = from_parts(services, parts, body, Path(PathParams::new())).await?;
	let (request, auth) = authenticate(services, request, root).await?;

	if let Some(span) = request.parts.extensions.get::<tracing::Span>() {
		record_auth_context(span, &auth);
	}

	let upstream = upstream_request(request, root, &auth)?;
	let response = services
		.appservice
		.send_proxied(&appservice, upstream)
		.await
		.map_err(|_| {
			Error::Request(
				ErrorKind::Unknown,
				"Appservice unreachable.".into(),
				StatusCode::BAD_GATEWAY,
			)
		})?;

	Ok(downstream_response(response))
}

/// The API and registration a request is proxied to.
///
/// Every method but TRACE qualifies, since the MSC forwards any authenticated
/// request under a claim, while a TRACE must carry no credential its echo could
/// disclose (RFC 9110 §9.3.8) and the forwarded request carries the `hs_token`.
/// A path qualifies when an appservice validly claims its remainder after the
/// version.
#[tracing::instrument(level = "trace", skip_all)]
async fn claim(
	services: &Services,
	method: &Method,
	uri: &Uri,
) -> Option<(Root, RegistrationInfo)> {
	method.ne(&Method::TRACE).into_option()?;

	let (root, remainder) = split_path(uri.path())?;
	let appservice = services
		.appservice
		.find_proxy_claim(remainder)
		.await?;

	Some((root, appservice))
}

/// The 404 every unmatched and unclaimed request receives.
///
/// It is the same answer as for an endpoint this server lacks, so a probe
/// cannot tell an unclaimed path from an unknown one.
fn not_found() -> Error {
	Error::Request(ErrorKind::Unrecognized, "Not Found".into(), StatusCode::NOT_FOUND)
}

/// Authenticates a proxied request as the claimed API's native routes do.
///
/// A client path takes an access token, an appservice's masquerade included;
/// a federation path takes a server signature over the canonical body, which
/// comes back as the content the signature covered.
#[tracing::instrument(level = "debug", skip_all)]
async fn authenticate(
	services: &Services,
	mut request: Request,
	root: Root,
) -> Result<(Request, Auth)> {
	let route = TypeId::of::<Proxied>();
	let auth = match root {
		| Root::Client => auth::<AccessToken>(services, &mut request, route).await?,
		| Root::Federation => auth::<ServerSignatures>(services, &mut request, route).await?,
	};
	Ok((request, auth))
}

/// The request sent on to the appservice, before its credentials are added.
///
/// The path is the original raw one. A client request's body goes as
/// received; a federation request's goes as the canonical content its
/// signature covered, so the appservice reads what was verified. The caller's
/// access token, cookies, other headers and any header its `Connection`
/// names stay here; the identity it authenticated as rides in an MSC4512
/// header.
fn upstream_request(request: Request, root: Root, auth: &Auth) -> Result<HttpRequest<Bytes>> {
	let identity = match root {
		| Root::Client => auth
			.sender_user
			.as_ref()
			.map(|user| (USER_IDENTIFIER, user.as_str())),
		| Root::Federation => auth
			.origin
			.as_ref()
			.map(|origin| (ORIGIN, origin.as_str())),
	};

	let (name, value) =
		identity.ok_or_else(|| err!(Request(Forbidden("Proxied request has no identity."))))?;

	let body = match root {
		| Root::Client => request.body,
		| Root::Federation => auth
			.signed_content
			.as_ref()
			.map(serialize_body)
			.unwrap_or_default(),
	};

	let headers = &request.parts.headers;
	let builder = HttpRequest::builder()
		.method(request.parts.method.clone())
		.uri(upstream_path(&request.parts.uri));

	FORWARDED_HEADERS
		.iter()
		.filter(|name| !listed_in_connection(headers, name.as_str()))
		.flat_map(|name| {
			headers
				.get_all(name)
				.iter()
				.map(move |value| (name, value))
		})
		.fold(builder, |builder, (name, value)| builder.header(name, value))
		.header(name, value)
		.body(body)
		.map_err(|e| err!(Request(InvalidParam("Proxied request is malformed: {e}"))))
}

/// The raw path and query, without any query pair carrying an access token.
///
/// Keys are compared decoded, as the token is read from them; every other
/// pair is kept byte for byte, and empty ones are dropped.
fn upstream_path(uri: &Uri) -> String {
	let pairs = uri
		.query()
		.unwrap_or_default()
		.split('&')
		.filter(|pair| !pair.is_empty() && !is_access_token(pair));

	pairs
		.clone()
		.next()
		.map(|_| format!("{}?{}", uri.path(), pairs.format("&")))
		.unwrap_or_else(|| uri.path().to_owned())
}

fn is_access_token(pair: &str) -> bool {
	parse_query(pair.as_bytes())
		.next()
		.is_some_and(|(key, _)| key == "access_token")
}

/// The appservice's response, relayed with its body streaming.
///
/// The status passes through unchanged; the headers lose those
/// [`relayed_headers`] withholds.
fn downstream_response(response: ReqwestResponse) -> Response {
	let (parts, body) = HttpResponse::<ReqwestBody>::from(response).into_parts();
	let headers = relayed_headers(parts.headers);

	(parts.status, headers, Body::new(body)).into_response()
}

/// The response headers this server relays.
///
/// Those [`DROPPED_HEADERS`] names go, as do every `access-control-*` one and
/// any the response's `Connection` header lists.
fn relayed_headers(mut headers: HeaderMap) -> HeaderMap {
	let dropped: Vec<HeaderName> = headers
		.keys()
		.filter(|name| is_dropped(&headers, name))
		.cloned()
		.collect();

	for name in &dropped {
		headers.remove(name);
	}

	headers
}

fn is_dropped(headers: &HeaderMap, name: &HeaderName) -> bool {
	let name = name.as_str();

	DROPPED_HEADERS.contains(&name)
		|| name.starts_with("access-control-")
		|| listed_in_connection(headers, name)
}

/// Whether a message's `Connection` header names `name`.
///
/// A header so named is hop-by-hop for that message (RFC 9110 §7.6.1), so it
/// is never passed on.
fn listed_in_connection(headers: &HeaderMap, name: &str) -> bool {
	headers
		.get_all(CONNECTION)
		.iter()
		.filter_map(|value| value.to_str().ok())
		.flat_map(|value| value.split(','))
		.any(|listed| listed.trim().eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
	use http::{HeaderMap, HeaderName, HeaderValue, Uri};
	use itertools::Itertools;

	use super::{relayed_headers, upstream_path};

	#[test]
	fn upstream_path_drops_every_access_token_pair() {
		for (uri, expected) in [
			("/p", "/p"),
			("/p?", "/p"),
			("/p?access_token=t", "/p"),
			("/p?access_token", "/p"),
			("/p?a=1&access_token=t&b=%20+", "/p?a=1&b=%20+"),
			("/p?access%5Ftoken=t&a=1", "/p?a=1"),
			("/p?access_tokens=t", "/p?access_tokens=t"),
			("/p?a=%2F&a=2", "/p?a=%2F&a=2"),
			("/p?&a=1&&b=2&", "/p?a=1&b=2"),
		] {
			let uri: Uri = uri.parse().expect("valid uri");

			assert_eq!(upstream_path(&uri), expected, "{uri}");
		}
	}

	#[test]
	fn relayed_headers_drop_hop_by_hop_and_server_policy() {
		let headers: HeaderMap = [
			("connection", "x-hop"),
			("x-hop", "1"),
			("keep-alive", "timeout=5"),
			("transfer-encoding", "chunked"),
			("set-cookie", "a=b"),
			("access-control-allow-origin", "*"),
			("content-security-policy", "default-src 'none'"),
			("x-frame-options", "DENY"),
			("content-type", "application/json"),
			("x-kept", "1"),
		]
		.into_iter()
		.map(|(name, value)| (name.parse().expect("name"), HeaderValue::from_static(value)))
		.collect();

		let relayed = relayed_headers(headers);
		let names: Vec<_> = relayed
			.keys()
			.map(HeaderName::as_str)
			.sorted_unstable()
			.collect();

		assert_eq!(names, ["content-type", "x-kept"]);
	}
}
