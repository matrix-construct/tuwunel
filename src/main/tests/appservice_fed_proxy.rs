#![cfg(test)]

use std::{net::TcpListener, path::PathBuf};

use axum::{
	Router,
	body::{Bytes, to_bytes},
	extract::{Request, State},
	http::{StatusCode, header::CONTENT_TYPE},
	response::IntoResponse,
};
use axum_server::{from_tcp_rustls, tls_rustls::RustlsConfig};
use reqwest::{Method, Response};
use serde_json::{Value, json};
use tokio::{
	sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
	task::JoinSet,
};
use tuwunel_core::{
	Result, err,
	http::{HeaderValue, Request as HttpRequest},
	ruma::{
		ServerName,
		api::{
			appservice::{Namespaces, Registration, RegistrationInit},
			federation::authentication::{ServerSignaturesInput, XMatrix},
		},
	},
};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
};

#[expect(dead_code)] // Only registration and raw posts are used from the client harness.
mod client;
mod fixture;

const CERTIFICATE: &str = "../../nix/pkgs/complement/certificate.crt";
const PRIVATE_KEY: &str = "../../nix/pkgs/complement/private_key.key";

const USER_TOKEN: &str = "appservice-fed-proxy-user-access-token-01";
const AS_TOKEN: &str = "appservice-fed-proxy-as-token";
const UNCLAIMED_TOKEN: &str = "appservice-fed-proxy-unclaimed-token";
const FORBIDDEN: &str = "forbidden.invalid";
const GET_TOKEN: &str = "/_matrix/federation/v1/rtc/livekit/get_token";
const FED_PROXY: &str = "unstable/io.element.msc4512/";

struct Seen {
	method: Method,
	path_and_query: String,
	authorization: Option<HeaderValue>,
	body: Bytes,
}

/// MSC4512: `fed_proxy` signs an appservice's request as this server and
/// relays the remote answer.
///
/// A request under the appservice's claimed prefix reaches a TLS peer signed
/// over the path and the bytes the peer received, and the peer's status and
/// JSON object body come back whatever the status. A path outside the claim,
/// a destination this server does not federate with, a malformed method and a
/// caller that is not an appservice are each refused with the MSC's code.
#[test]
fn fed_proxy_relays_signed_requests() -> Result {
	let options = [
		"network.http.allow_invalid_tls_certificates=true",
		"network.http.ip_range_denylist=[]",
		"federation.loopback=true",
		r#"federation.forbidden_remote_server_names=["^forbidden\\.invalid$"]"#,
	];

	boot("appservice-fed-proxy", options, exercise)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "fed_proxy_user", USER_TOKEN).await?;
	register_appservice(services, "lk", AS_TOKEN, Some("rtc/livekit")).await?;
	register_appservice(services, "unclaimed", UNCLAIMED_TOKEN, None).await?;

	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let peer = format!("127.0.0.1:{}", listener.local_addr()?.port());
	let peer = ServerName::parse(&peer).map_err(|e| err!("peer server name: {e}"))?;

	listener.set_nonblocking(true)?;

	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let config =
		RustlsConfig::from_pem_file(manifest.join(CERTIFICATE), manifest.join(PRIVATE_KEY))
			.await?;

	let (tx, mut seen) = unbounded_channel();
	let app = Router::new().fallback(remote).with_state(tx);
	let remote = from_tcp_rustls(listener, config)?.serve(app.into_make_service());
	let mut tasks = JoinSet::new(); // The set aborts the peer when the exercise exits.

	tasks.spawn_on(remote, services.server.runtime());

	let appservice = Client { services, base, token: AS_TOKEN };
	let body = json!({ "user_id": "@alice:remote.invalid", "room_id": "!room:remote.invalid" });
	let relayed = fed_proxy(
		&appservice,
		&json!({ "destination": peer, "method": "POST", "path": GET_TOKEN, "body": body }),
	)
	.await?;

	assert_eq!(relayed, (200, json!({ "status": 200, "content": { "jwt": "token" } })));

	let request = received(&mut seen)?;

	assert_eq!(request.method, Method::POST);
	assert_eq!(request.path_and_query, GET_TOKEN);
	assert_eq!(serde_json::from_slice::<Value>(&request.body)?, body);
	assert_eq!(request.authorization, Some(signature(services, &peer, &request)?));

	// The fraction leaves the outer body non-canonical, so its text reaches the
	// handler as written.
	let unsorted = format!(
		r#"{{"destination":"{peer}","method":"POST","path":"{GET_TOKEN}","body":{{"z":1,"a":[2,{{"y":0,"b":1}}]}},"extra":0.5}}"#
	);

	let response = services
		.client
		.clients
		.default
		.post(fed_proxy_url(&appservice))
		.bearer_auth(AS_TOKEN)
		.header("content-type", "application/json")
		.body(unsorted)
		.send()
		.await?;

	assert_eq!(response.status(), StatusCode::OK);

	let request = received(&mut seen)?;

	assert_eq!(request.body, &br#"{"a":[2,{"b":1,"y":0}],"z":1}"#[..]);
	assert_eq!(request.authorization, Some(signature(services, &peer, &request)?));

	let query = json!({ "b": "x", "a": "1 2" });
	let relayed = fed_proxy(
		&appservice,
		&json!({ "destination": peer, "method": "GET", "path": GET_TOKEN, "query": query }),
	)
	.await?;

	assert_eq!(relayed.0, 200);

	let request = received(&mut seen)?;

	assert_eq!(request.method, Method::GET);
	assert_eq!(request.path_and_query, format!("{GET_TOKEN}?a=1+2&b=x"));
	assert!(request.body.is_empty());
	assert_eq!(request.authorization, Some(signature(services, &peer, &request)?));

	let missing = json!({ "errcode": "M_UNRECOGNIZED", "error": "Unrecognized request" });
	let broken = json!({ "errcode": "M_UNKNOWN", "error": "Internal error" });

	for (suffix, expected) in [
		("missing", json!({ "status": 404, "content": missing })),
		("broken", json!({ "status": 500, "content": broken })),
		("plain", json!({ "status": 200 })),
		("array", json!({ "status": 200 })),
		("accepted", json!({ "status": 202, "content": {} })),
	] {
		let path = format!("/_matrix/federation/v1/rtc/livekit/{suffix}");
		let relayed = fed_proxy(
			&appservice,
			&json!({ "destination": peer, "method": "POST", "path": path, "body": {} }),
		)
		.await?;

		assert_eq!(relayed, (200, expected), "{suffix}");
		received(&mut seen)?;
	}

	let closed = TcpListener::bind(("127.0.0.1", 0))?.local_addr()?;
	let ours = services.globals.server_name();

	for (destination, expected) in [
		(closed.to_string(), (502, "IO.ELEMENT.MSC4512_FEDPROXY_CONNECTION_FAILED")),
		(ours.to_string(), (403, "IO.ELEMENT.MSC4512_FEDPROXY_DESTINATION_DENIED")),
		(FORBIDDEN.to_owned(), (403, "IO.ELEMENT.MSC4512_FEDPROXY_DESTINATION_DENIED")),
	] {
		let request = json!({ "destination": destination, "method": "GET", "path": GET_TOKEN });
		let (status, errcode) = refusal(&appservice, &request).await?;

		assert_eq!((status, errcode.as_str()), expected, "{destination}");
	}

	let path_not_allowed = (403, "IO.ELEMENT.MSC4512_FEDPROXY_PATH_NOT_ALLOWED");

	for path in [
		"/_matrix/federation/v1/rtc/livekit/../x",
		"/_matrix/federation/v1/rtc/livekit/%2e%2E/x",
		"/_matrix/federation/v1/rtc/livekit/get_token?a=1",
		"/_matrix/federation/v1/rtc/livekit/get_token#a",
		"/_matrix/federation/v1/rtc/livekit/a\\b",
		"/_matrix/federation/v1/rtc/livekit/a b",
		"/_matrix/federation/v1/rtc/other",
		"/_matrix/federation/v1/rtc/livekit-2/get_token",
		"/_matrix/federation/rtc/livekit/get_token",
		"/_matrix/client/v1/rtc/livekit/get_token",
	] {
		let request = json!({ "destination": peer, "method": "GET", "path": path });
		let (status, errcode) = refusal(&appservice, &request).await?;

		assert_eq!((status, errcode.as_str()), path_not_allowed, "{path}");
	}

	let unclaimed = Client { services, base, token: UNCLAIMED_TOKEN };
	let request = json!({ "destination": peer, "method": "GET", "path": GET_TOKEN });
	let (status, errcode) = refusal(&unclaimed, &request).await?;

	assert_eq!((status, errcode.as_str()), path_not_allowed, "an appservice without a claim");

	for (method, body) in [
		("GET", Some(json!({}))),
		("DELETE", Some(json!({}))),
		("PATCH", None),
		("POST", Some(json!([]))),
		("POST", Some(json!("body"))),
		("PUT", Some(json!(1))),
		("POST", Some(json!(true))),
		("POST", Some(json!({ "fraction": 0.5 }))),
	] {
		let request = match &body {
			| Some(body) =>
				json!({ "destination": peer, "method": method, "path": GET_TOKEN, "body": body }),
			| None => json!({ "destination": peer, "method": method, "path": GET_TOKEN }),
		};

		let (status, errcode) = refusal(&appservice, &request).await?;

		assert_eq!((status, errcode.as_str()), (400, "M_INVALID_PARAM"), "{method} {body:?}");
	}

	let user = Client { services, base, token: USER_TOKEN };
	let ping = user.client_url("v1/", "appservice/lk/ping");

	for url in [fed_proxy_url(&user), ping] {
		let (status, errcode) = refused(user.post_url(&url, &request).await?).await?;

		assert_eq!((status, errcode.as_str()), (403, "M_FORBIDDEN"), "{url}");
	}

	let anonymous = services
		.client
		.clients
		.default
		.post(fed_proxy_url(&user))
		.json(&request)
		.send()
		.await?;

	assert_eq!(refused(anonymous).await?, (401, "M_MISSING_TOKEN".into()));

	assert!(seen.try_recv().is_err(), "a refused request reached the peer");

	Ok(())
}

/// Records the request and answers it by its last path segment.
///
/// `get_token` answers with a token, `missing` 404, `broken` 500, `accepted`
/// 202, `plain` a non-JSON body and `array` a JSON array.
#[tracing::instrument(level = "debug", skip_all)]
async fn remote(
	State(seen): State<UnboundedSender<Seen>>,
	request: Request,
) -> impl IntoResponse {
	let (parts, body) = request.into_parts();
	let body = to_bytes(body, usize::MAX)
		.await
		.unwrap_or_default();

	let last = parts
		.uri
		.path()
		.rsplit('/')
		.next()
		.unwrap_or_default()
		.to_owned();

	let path_and_query = parts
		.uri
		.path_and_query()
		.map(ToString::to_string)
		.unwrap_or_default();

	seen.send(Seen {
		method: parts.method,
		path_and_query,
		authorization: parts.headers.get("authorization").cloned(),
		body,
	})
	.ok();

	let (status, content_type, body) = match last.as_str() {
		| "missing" => (
			StatusCode::NOT_FOUND,
			"application/json",
			r#"{"errcode":"M_UNRECOGNIZED","error":"Unrecognized request"}"#,
		),
		| "broken" => (
			StatusCode::INTERNAL_SERVER_ERROR,
			"application/json",
			r#"{"errcode":"M_UNKNOWN","error":"Internal error"}"#,
		),
		| "plain" => (StatusCode::OK, "text/plain", "not json"),
		| "accepted" => (StatusCode::ACCEPTED, "application/json", "{}"),
		| "array" => (StatusCode::OK, "application/json", "[1]"),
		| _ => (StatusCode::OK, "application/json", r#"{"jwt":"token"}"#),
	};

	(status, [(CONTENT_TYPE, content_type)], body)
}

/// The `X-Matrix` header this server would sign the received request with.
///
/// Ed25519 signatures are deterministic, so equality with the header the peer
/// got proves the signature covers the path and bytes it received.
fn signature(services: &Services, peer: &ServerName, seen: &Seen) -> Result<HeaderValue> {
	let ours = services.globals.server_name();
	let request = HttpRequest::builder()
		.method(seen.method.clone())
		.uri(&seen.path_and_query)
		.body(seen.body.as_ref())?;

	let input = ServerSignaturesInput::new(
		ours.to_owned(),
		peer.to_owned(),
		services.server_keys.keypair(),
	);

	let signature = XMatrix::try_from_http_request(&request, input)
		.map_err(|e| err!("signing the received request failed: {e}"))?;

	Ok(HeaderValue::from(&signature))
}

/// Register an appservice shaped like lk-jwt-service's registration.
///
/// It claims the proxy prefix when one is given. Nothing listens at its proxy
/// URL, since only `fed_proxy` is exercised here.
#[tracing::instrument(level = "debug", skip_all)]
async fn register_appservice(
	services: &Services,
	id: &str,
	as_token: &str,
	prefix: Option<&str>,
) -> Result {
	let base: Registration = RegistrationInit {
		id: id.to_owned(),
		url: None,
		as_token: as_token.to_owned(),
		hs_token: format!("{id}-hs-token"),
		sender_localpart: format!("{id}_bot"),
		namespaces: Namespaces::new(),
		rate_limited: None,
		protocols: None,
	}
	.into();

	let registration = Registration {
		proxy_prefix: prefix.map(ToOwned::to_owned),
		proxy_url: prefix.map(|_| "http://127.0.0.1:9".to_owned()),
		..base
	};

	services
		.appservice
		.register_appservice(registration)
		.await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn fed_proxy(client: &Client<'_>, request: &Value) -> Result<(u16, Value)> {
	let response = client
		.post_url(&fed_proxy_url(client), request)
		.await?;

	Ok((response.status().as_u16(), response.json().await?))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn refusal(client: &Client<'_>, request: &Value) -> Result<(u16, String)> {
	let response = client
		.post_url(&fed_proxy_url(client), request)
		.await?;

	refused(response).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn refused(response: Response) -> Result<(u16, String)> {
	let status = response.status().as_u16();
	let body: Value = response.json().await?;

	Ok((status, field(&body, "errcode")?.to_owned()))
}

fn fed_proxy_url(client: &Client<'_>) -> String {
	client.client_url(FED_PROXY, "appservice/fed_proxy")
}

fn received(seen: &mut UnboundedReceiver<Seen>) -> Result<Seen> {
	seen.try_recv()
		.map_err(|e| err!("the peer received nothing: {e}"))
}
