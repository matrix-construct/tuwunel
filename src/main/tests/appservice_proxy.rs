#![cfg(test)]

use std::net::TcpListener;

use axum::{
	Router,
	body::{Bytes, to_bytes},
	extract::{Request, State},
	http::{HeaderMap, StatusCode},
	response::IntoResponse,
};
use axum_server::from_tcp;
use reqwest::{Method, Response};
use serde_json::Value;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpStream,
	sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
	task::JoinSet,
};
use tuwunel_core::{
	Result, err,
	http::{HeaderValue, Request as HttpRequest, header::AUTHORIZATION},
	ruma::{
		UserId,
		api::{
			appservice::{Namespace, Namespaces, Registration, RegistrationInit},
			federation::authentication::{ServerSignaturesInput, XMatrix},
		},
	},
};
use tuwunel_service::Services;

use self::{
	client::{field, register},
	fixture::boot,
};

#[expect(dead_code)] // Only registration is used from the client harness.
mod client;
mod fixture;

const USER_TOKEN: &str = "appservice-proxy-user-access-token-0001";
const AS_TOKEN: &str = "appservice-proxy-as-token";
const HS_TOKEN: &str = "appservice-proxy-hs-token";

// Sent with unsorted keys, so a re-serialized body would differ.
const CLIENT_BODY: &str = r#"{"b":1,"a":2}"#;

// Canonical, since a server-signed body must be.
const FEDERATION_BODY: &str = r#"{"a":2,"b":1}"#;

const PROBE: &str =
	"/_matrix/client/unstable/io.element.msc4195/rtc/livekit/delegate_delayed_leave";

const GET_TOKEN: &str = "/_matrix/client/v1/rtc/livekit/get_token";
const FEDERATION_GET_TOKEN: &str = "/_matrix/federation/v1/rtc/livekit/get_token";

// Headers the stand-in answers with; only the first two may reach a client.
const STAND_IN_HEADERS: [(&str, &str); 8] = [
	("content-type", "application/json"),
	("x-relayed", "yes"),
	("set-cookie", "session=stand-in"),
	("access-control-allow-origin", "https://stand-in.invalid"),
	("content-security-policy", "script-src https://stand-in.invalid"),
	("x-frame-options", "ALLOW-FROM https://stand-in.invalid"),
	("connection", "x-hop"),
	("x-hop", "1"),
];

struct Seen {
	method: Method,
	uri: String,
	headers: HeaderMap,
	body: Bytes,
}

/// MSC4512: a claimed path is authenticated here and forwarded verbatim.
///
/// Client and federation requests under `rtc/livekit` reach a stand-in
/// appservice with the original path, the body as sent, the `hs_token` and the
/// caller's identity, but none of the caller's credentials or other headers;
/// its answer comes back without the headers that are this server's own.
/// Unclaimed paths stay a plain 404 before authentication, and a claimed path
/// without a token is 401, which Element Call reads as support.
#[test]
fn claimed_paths_reach_the_appservice() -> Result { boot("appservice-proxy", [], exercise) }

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	let user_id = register(services, "proxy_user", USER_TOKEN).await?;
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let stand_in_url = format!("http://{}/", listener.local_addr()?);

	listener.set_nonblocking(true)?;

	let (tx, mut seen) = unbounded_channel();
	let app = Router::new().fallback(stand_in).with_state(tx);
	let stand_in = from_tcp(listener)?.serve(app.into_make_service());
	let mut tasks = JoinSet::new(); // The set aborts the stand-in when the exercise exits.

	tasks.spawn_on(stand_in, services.server.runtime());

	let probe = format!("{base}{PROBE}");

	assert_eq!(
		refusal(send(services, Method::POST, &probe, None).await?).await?,
		(404, "M_UNRECOGNIZED".into())
	);

	register_appservice(services, "lk", AS_TOKEN, "rtc/livekit/", &stand_in_url).await?;

	let duplicate =
		register_appservice(services, "lk2", "lk2-token", "rtc/livekit", &stand_in_url).await;

	let unallowed =
		register_appservice(services, "lk3", "lk3-token", "send", &stand_in_url).await;

	assert!(duplicate.is_err(), "a second claim on the prefix was registered");
	assert!(unallowed.is_err(), "a claim outside the allowlist was registered");

	assert_eq!(
		refusal(send(services, Method::POST, &probe, None).await?).await?,
		(401, "M_MISSING_TOKEN".into())
	);

	let unknown = send(services, Method::POST, &probe, Some("not-a-token")).await?;

	let query_token = format!("{probe}?access_token={USER_TOKEN}");
	let by_query = send(services, Method::POST, &query_token, None).await?;

	assert_eq!(refusal(unknown).await?, (401, "M_UNKNOWN_TOKEN".into()));
	assert_eq!(refusal(by_query).await?, (401, "M_MISSING_TOKEN".into()));
	assert!(seen.try_recv().is_err(), "an unauthenticated request was forwarded");

	client_requests(services, base, &user_id, &mut seen).await?;
	refused_requests(services, base, &mut seen).await?;
	federation_requests(services, base, &mut seen).await?;

	// The stand-in's open connections outlive its listener, so aim at a closed port.
	let closed = TcpListener::bind(("127.0.0.1", 0))?.local_addr()?;

	register_appservice(services, "lk", AS_TOKEN, "rtc/livekit", &format!("http://{closed}"))
		.await?;

	let unreachable =
		send(services, Method::POST, &format!("{base}{GET_TOKEN}"), Some(USER_TOKEN)).await?;

	assert_eq!(refusal(unreachable).await?, (502, "M_UNKNOWN".into()));

	services
		.appservice
		.unregister_appservice("lk")
		.await?;

	assert_eq!(
		refusal(send(services, Method::POST, &probe, None).await?).await?,
		(404, "M_UNRECOGNIZED".into())
	);

	Ok(())
}

/// Client requests reach the stand-in with the caller's identity.
///
/// Nothing else of the caller's goes with them, and the answer comes back
/// without the headers that are this server's own.
#[tracing::instrument(level = "debug", skip_all)]
async fn client_requests(
	services: &Services,
	base: &str,
	user_id: &UserId,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	let ours = services.globals.server_name();
	let url = format!("{base}{GET_TOKEN}?a=1&access_token=ignored&b=%20");
	let response = services
		.client
		.clients
		.default
		.post(&url)
		.bearer_auth(USER_TOKEN)
		.header("content-type", "application/json")
		.header("accept-language", "en")
		.header("cookie", "session=caller")
		.header("x-forwarded-for", "192.0.2.1")
		.body(CLIENT_BODY)
		.send()
		.await?;

	assert_eq!(response.status(), StatusCode::OK);
	assert_eq!(header(response.headers(), "x-relayed"), Some("yes"));

	STAND_IN_HEADERS
		.into_iter()
		.skip(2)
		.for_each(|(name, value)| {
			assert_ne!(header(response.headers(), name), Some(value), "{name} was relayed");
		});

	assert_eq!(response.text().await?, r#"{"proxied":true}"#);

	let request = received(seen)?;

	assert_eq!(request.method, Method::POST);
	assert_eq!(request.uri, format!("{GET_TOKEN}?a=1&b=%20"));
	assert_eq!(request.body, CLIENT_BODY.as_bytes());
	assert_eq!(
		header(&request.headers, "authorization"),
		Some(format!("Bearer {HS_TOKEN}").as_str())
	);

	assert_eq!(header(&request.headers, "x-matrix-user-identifier"), Some(user_id.as_str()));
	assert_eq!(header(&request.headers, "accept-language"), Some("en"));
	assert_eq!(header(&request.headers, "content-type"), Some("application/json"));

	for name in ["cookie", "x-forwarded-for", "x-matrix-origin"] {
		assert_eq!(header(&request.headers, name), None, "{name} was forwarded");
	}

	let alice = format!("@lk_alice:{ours}");
	let bot = format!("@lk_bot:{ours}");
	let masquerade = format!("{GET_TOKEN}?user_id={alice}");
	let encoded = format!("{GET_TOKEN}?access%5Ftoken={USER_TOKEN}&c=3");
	let encoded_upstream = format!("{GET_TOKEN}?c=3");

	for (path, token, uri, identity) in [
		(PROBE, Some(USER_TOKEN), PROBE, user_id.as_str()),
		(
			"/_matrix/client/v1/rtc/livekit",
			Some(USER_TOKEN),
			"/_matrix/client/v1/rtc/livekit",
			user_id.as_str(),
		),
		(encoded.as_str(), Some(USER_TOKEN), encoded_upstream.as_str(), user_id.as_str()),
		(GET_TOKEN, Some(AS_TOKEN), GET_TOKEN, bot.as_str()),
		(masquerade.as_str(), Some(AS_TOKEN), masquerade.as_str(), alice.as_str()),
	] {
		let response = send(services, Method::POST, &format!("{base}{path}"), token).await?;

		assert_eq!(response.status(), StatusCode::OK, "{path}");

		let request = received(seen)?;

		assert_eq!(request.uri, uri, "{path}");
		assert_eq!(
			header(&request.headers, "x-matrix-user-identifier"),
			Some(identity),
			"{path}"
		);
	}

	for method in [Method::GET, Method::PUT, Method::DELETE, Method::PATCH] {
		let response =
			send(services, method.clone(), &format!("{base}{GET_TOKEN}"), Some(USER_TOKEN))
				.await?;

		assert_eq!(response.status(), StatusCode::OK, "{method}");

		let request = received(seen)?;

		assert_eq!((request.method, request.body.as_ref()), (method, CLIENT_BODY.as_bytes()));
	}

	let (status, _) = raw_post(base, GET_TOKEN, ", accept-language").await?;
	let request = received(seen)?;

	assert_eq!(status, 200);
	assert_eq!(
		header(&request.headers, "accept-language"),
		None,
		"a nominated header was forwarded"
	);

	let teapot = format!("{base}/_matrix/client/v1/rtc/livekit/teapot");
	let response = send(services, Method::POST, &teapot, Some(USER_TOKEN)).await?;

	assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
	received(seen)?;

	Ok(())
}

/// Requests the proxy refuses never reach the stand-in.
///
/// Unclaimed paths and a TRACE under the claim are the plain 404, a known
/// path's wrong method the plain 405, and a dot segment under the claim is 400.
#[tracing::instrument(level = "debug", skip_all)]
async fn refused_requests(
	services: &Services,
	base: &str,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	for path in [
		"/_matrix/client/v1/rtc/livekit-2/get_token",
		"/_matrix/client/v1/other/get_token",
		"/_matrix/client/rtc/livekit/get_token",
		"/_matrix/media/v1/rtc/livekit/get_token",
	] {
		let response = send(services, Method::POST, &format!("{base}{path}"), None).await?;

		assert_eq!(refusal(response).await?, (404, "M_UNRECOGNIZED".into()), "{path}");
	}

	let trace =
		send(services, Method::TRACE, &format!("{base}{GET_TOKEN}"), Some(USER_TOKEN)).await?;

	let wrong_method = send(
		services,
		Method::DELETE,
		&format!("{base}/_matrix/client/v3/createRoom"),
		Some(USER_TOKEN),
	)
	.await?;

	assert_eq!(refusal(trace).await?, (404, "M_UNRECOGNIZED".into()));
	assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);

	for path in [
		"/_matrix/client/v1/rtc/livekit/../x",
		"/_matrix/client/v1/rtc/livekit/%2e%2E/x",
		"/_matrix/client/v1/rtc/livekit/x\\..\\..\\other",
	] {
		let (status, body) = raw_post(base, path, "").await?;

		assert_eq!(status, 400, "{path}");
		assert!(body.contains("M_INVALID_PARAM"), "{path}: {body}");
	}

	assert!(seen.try_recv().is_err(), "a refused request was forwarded");

	Ok(())
}

/// A federation request signed as a peer signs it reaches the stand-in.
///
/// It carries the origin it was signed by; one whose signature does not
/// cover its body is refused.
#[tracing::instrument(level = "debug", skip_all)]
async fn federation_requests(
	services: &Services,
	base: &str,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	let ours = services.globals.server_name();
	let response = federation_post(services, base, FEDERATION_BODY, FEDERATION_BODY).await?;

	assert_eq!(response.status(), StatusCode::OK);

	let request = received(seen)?;

	assert_eq!(request.uri, FEDERATION_GET_TOKEN);
	assert_eq!(request.body, FEDERATION_BODY.as_bytes());
	assert_eq!(
		header(&request.headers, "authorization"),
		Some(format!("Bearer {HS_TOKEN}").as_str())
	);

	assert_eq!(header(&request.headers, "x-matrix-origin"), Some(ours.as_str()));
	assert_eq!(header(&request.headers, "x-matrix-user-identifier"), None);

	let duplicated = federation_post(services, base, r#"{"a":1}"#, r#"{"a":2,"a":1}"#).await?;

	assert_eq!(duplicated.status(), StatusCode::OK);
	assert_eq!(received(seen)?.body, &br#"{"a":1}"#[..], "the verified content was not sent");

	let forged = federation_post(services, base, FEDERATION_BODY, r#"{"a":3,"b":1}"#).await?;

	assert_eq!(refusal(forged).await?, (403, "M_FORBIDDEN".into()));
	assert!(seen.try_recv().is_err(), "a forged federation request was forwarded");

	Ok(())
}

/// Records the request and answers with [`STAND_IN_HEADERS`].
///
/// A path ending in `teapot` answers 418, so a refusal's status can be seen
/// relayed.
#[tracing::instrument(level = "debug", skip_all)]
async fn stand_in(
	State(seen): State<UnboundedSender<Seen>>,
	request: Request,
) -> impl IntoResponse {
	let (parts, body) = request.into_parts();
	let body = to_bytes(body, usize::MAX)
		.await
		.unwrap_or_default();

	let status = match parts.uri.path().ends_with("/teapot") {
		| true => StatusCode::IM_A_TEAPOT,
		| false => StatusCode::OK,
	};

	seen.send(Seen {
		method: parts.method,
		uri: parts.uri.to_string(),
		headers: parts.headers,
		body,
	})
	.ok();

	(status, STAND_IN_HEADERS, r#"{"proxied":true}"#)
}

/// Register an appservice claiming `prefix`.
///
/// It is shaped like lk-jwt-service's registration: no URL, and every local
/// user claimed without exclusivity.
#[tracing::instrument(level = "debug", skip_all)]
async fn register_appservice(
	services: &Services,
	id: &str,
	as_token: &str,
	prefix: &str,
	proxy_url: &str,
) -> Result {
	let base: Registration = RegistrationInit {
		id: id.to_owned(),
		url: None,
		as_token: as_token.to_owned(),
		hs_token: HS_TOKEN.to_owned(),
		sender_localpart: format!("{id}_bot"),
		namespaces: Namespaces {
			users: vec![Namespace::new(false, ".*".to_owned())],
			..Default::default()
		},
		rate_limited: None,
		protocols: None,
	}
	.into();

	let registration = Registration {
		proxy_prefix: Some(prefix.to_owned()),
		proxy_url: Some(proxy_url.to_owned()),
		..base
	};

	services
		.appservice
		.register_appservice(registration)
		.await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn send(
	services: &Services,
	method: Method,
	url: &str,
	token: Option<&str>,
) -> Result<Response> {
	let request = services
		.client
		.clients
		.default
		.request(method, url)
		.header("content-type", "application/json")
		.body(CLIENT_BODY);

	let request = match token {
		| Some(token) => request.bearer_auth(token),
		| None => request,
	};

	let response = request.send().await?;

	Ok(response)
}

/// Post [`FEDERATION_GET_TOKEN`] as this server.
///
/// The signature covers `signed` while the body sent is `sent`, so a forged
/// body can be tried.
#[tracing::instrument(level = "debug", skip_all)]
async fn federation_post(
	services: &Services,
	base: &str,
	signed: &str,
	sent: &str,
) -> Result<Response> {
	let ours = services.globals.server_name();
	let uri = format!("{base}{FEDERATION_GET_TOKEN}");
	let request = HttpRequest::builder()
		.method("POST")
		.uri(&uri)
		.body(signed)?;

	let input = ServerSignaturesInput::new(
		ours.to_owned(),
		ours.to_owned(),
		services.server_keys.keypair(),
	);

	let signature = XMatrix::try_from_http_request(&request, input)
		.map_err(|e| err!("signing the request failed: {e}"))?;

	let response = services
		.client
		.clients
		.default
		.post(&uri)
		.header(AUTHORIZATION, HeaderValue::from(&signature))
		.header("content-type", "application/json")
		.body(sent.to_owned())
		.send()
		.await?;

	Ok(response)
}

/// Post to a path written verbatim and return the status and raw response.
///
/// Every URL parser removes dot segments on the client side, and a client may
/// rewrite `Connection`, so this one writes the request itself. `connection`
/// is appended to that header after `close`.
#[tracing::instrument(level = "debug", skip_all)]
async fn raw_post(base: &str, path: &str, connection: &str) -> Result<(u16, String)> {
	let address = base
		.strip_prefix("http://")
		.ok_or_else(|| err!("the base is not plain HTTP"))?;

	let request = format!(
		"POST {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer \
		 {USER_TOKEN}\r\nAccept-Language: en\r\nContent-Type: \
		 application/json\r\nContent-Length: 2\r\nConnection: close{connection}\r\n\r\n{{}}"
	);

	let response = exchange(TcpStream::connect(address).await?, &request).await?;
	let status = response
		.split(' ')
		.nth(1)
		.and_then(|code| code.parse().ok())
		.ok_or_else(|| err!("no status line in {response:?}"))?;

	Ok((status, response))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exchange(mut stream: TcpStream, request: &str) -> Result<String> {
	let mut response = String::new(); // read_to_string out-param

	stream.write_all(request.as_bytes()).await?;
	stream.read_to_string(&mut response).await?;

	Ok(response)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn refusal(response: Response) -> Result<(u16, String)> {
	let status = response.status().as_u16();
	let body: Value = response.json().await?;

	Ok((status, field(&body, "errcode")?.to_owned()))
}

fn received(seen: &mut UnboundedReceiver<Seen>) -> Result<Seen> {
	seen.try_recv()
		.map_err(|e| err!("the stand-in received nothing: {e}"))
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
	headers
		.get(name)
		.and_then(|value| value.to_str().ok())
}
