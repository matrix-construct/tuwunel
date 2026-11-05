//! MSC4388 rendezvous through the router.
//!
//! The routes come from ruma's path metadata and one literal send path, so a
//! lost path answers 404 at runtime and compiles everywhere; the guard against
//! it can only live here.

#![cfg(test)]

use reqwest::{Method, Response, StatusCode, header::CONTENT_TYPE};
use serde_json::{Value, json};
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, register},
	fixture::boot,
};

#[expect(
	dead_code,
	reason = "Only part of the client API harness is used here."
)]
mod client;
mod fixture;

const ROUTE: &str = "io.element.msc4388/rendezvous";
const TOKEN: &str = "rendezvous-msc4388-test-token-0123456789";
const BAD_TOKEN: &str = "rendezvous-msc4388-bad-token";
const PAYLOAD: &str = "second";
const QUIET: &str = "logging.enable=false";
const MALFORMED: [(&str, &str); 4] = [
	("not-json", "M_NOT_JSON"),
	("", "M_NOT_JSON"),
	(r#"{"data":0,"#, "M_NOT_JSON"),
	("{}", "M_BAD_JSON"),
];
const OIDC: [&str; 4] = [
	"well_known.client=\"https://localhost\"",
	"authentication.oidc.native_auth=true",
	// The exercise outruns the default per-client burst.
	"rendezvous.rc_burst_count=1000",
	QUIET,
];

#[test]
fn send_and_discovery_routes() -> Result {
	let disabled = OIDC
		.into_iter()
		.chain(["rendezvous.enable=false"]);

	let open = OIDC
		.into_iter()
		.chain(["rendezvous.authenticated_only=false"]);

	boot("rendezvous-msc4388", OIDC, exercise)
		.and_then(|()| boot("rendezvous-msc4388-no-oidc", [QUIET], unavailable))
		.and_then(|()| boot("rendezvous-msc4388-disabled", disabled, unavailable))
		.and_then(|()| boot("rendezvous-msc4388-open", open, open_creation))
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "rendezvous_device", TOKEN).await?;

	let user = Client { services, base, token: TOKEN };
	let url = route_url(services, base);

	discovery(services, &user, &url).await?;

	let created = user
		.request(Method::POST, &url, &json!({ "data": "first" }))
		.await?;

	let created = accepted(created).await?;
	let session = format!("{url}/{}", field(&created, "id")?);
	let initial = field(&created, "sequence_token")?;

	sends(services, &session, initial).await
}

fn route_url(services: &Services, base: &str) -> String {
	Client { services, base, token: "" }.client_url("unstable/", ROUTE)
}

async fn discovery(services: &Services, user: &Client<'_>, url: &str) -> Result {
	let nothing = json!({});
	let unauthenticated = anonymous(services, Method::GET, url, &nothing).await?;

	assert_eq!(accepted(unauthenticated).await?, json!({ "create_available": false }));

	let authenticated = user.request(Method::GET, url, &nothing).await?;

	assert_eq!(accepted(authenticated).await?, json!({ "create_available": true }));

	let stranger = Client { token: BAD_TOKEN, ..*user };
	let unknown = stranger
		.request(Method::GET, url, &nothing)
		.await?;

	assert_refused(unknown, StatusCode::UNAUTHORIZED, "M_UNKNOWN_TOKEN").await?;

	let forbidden = anonymous(services, Method::POST, url, &json!({ "data": "" })).await?;

	assert_refused(forbidden, StatusCode::FORBIDDEN, "M_FORBIDDEN").await
}

async fn sends(services: &Services, session: &str, initial: &str) -> Result {
	let oversized = "x".repeat(5000);
	let sent = send(services, session, "one", initial, PAYLOAD).await?;
	let sent = accepted(sent).await?;
	let current = field(&sent, "sequence_token")?;
	let stored = (PAYLOAD.to_owned(), current.to_owned());

	assert_ne!(current, initial);
	assert_eq!(read(services, session).await?, stored);

	for data in ["different", oversized.as_str()] {
		let replay = send(services, session, "one", initial, data).await?;

		assert_eq!(accepted(replay).await?, sent);
		assert_eq!(read(services, session).await?, stored);
	}

	let retried = format!("{session}/one");
	let unsent = format!("{session}/two");

	for (malformed, errcode) in MALFORMED {
		let replay = raw(services, Method::PUT, &retried, malformed).await?;

		assert_eq!(accepted(replay).await?, sent);

		let refused = raw(services, Method::PUT, &unsent, malformed).await?;

		assert_refused(refused, StatusCode::BAD_REQUEST, errcode).await?;
	}

	let conflict = send(services, session, "two", initial, PAYLOAD).await?;

	assert_refused(conflict, StatusCode::CONFLICT, "IO_ELEMENT_MSC4388_CONCURRENT_WRITE").await?;

	let too_large = send(services, session, "two", current, &oversized).await?;

	assert_refused(too_large, StatusCode::PAYLOAD_TOO_LARGE, "M_TOO_LARGE").await?;

	for malformed in ["a%20b", "%FF"] {
		let response = send(services, session, malformed, current, PAYLOAD).await?;

		assert_refused(response, StatusCode::BAD_REQUEST, "M_INVALID_PARAM").await?;
	}

	let bare = put(services, session, current, PAYLOAD).await?;

	assert_eq!(bare.status(), StatusCode::METHOD_NOT_ALLOWED);

	let duplicated = format!(r#"{{"sequence_token":"{current}","data":"old","data":"third"}}"#);
	let last = raw(services, Method::PUT, &unsent, &duplicated).await?;
	let last = accepted(last).await?;
	let stored = ("third".to_owned(), field(&last, "sequence_token")?.to_owned());

	assert_eq!(read(services, session).await?, stored);

	Ok(())
}

async fn send(
	services: &Services,
	session: &str,
	txn_id: &str,
	sequence_token: &str,
	data: &str,
) -> Result<Response> {
	put(services, &format!("{session}/{txn_id}"), sequence_token, data).await
}

async fn put(
	services: &Services,
	url: &str,
	sequence_token: &str,
	data: &str,
) -> Result<Response> {
	let body = json!({ "sequence_token": sequence_token, "data": data });

	anonymous(services, Method::PUT, url, &body).await
}

async fn read(services: &Services, session: &str) -> Result<(String, String)> {
	let response = anonymous(services, Method::GET, session, &json!({})).await?;
	let body = accepted(response).await?;
	let data = field(&body, "data")?.to_owned();
	let sequence_token = field(&body, "sequence_token")?.to_owned();

	Ok((data, sequence_token))
}

async fn unavailable(services: &Services, base: &str) -> Result {
	let url = route_url(services, base);
	let response = anonymous(services, Method::GET, &url, &json!({})).await?;

	assert_refused(response, StatusCode::NOT_FOUND, "M_UNRECOGNIZED").await
}

async fn open_creation(services: &Services, base: &str) -> Result {
	let url = route_url(services, base);
	let discovered = anonymous(services, Method::GET, &url, &json!({})).await?;

	assert_eq!(accepted(discovered).await?, json!({ "create_available": true }));

	let created = anonymous(services, Method::POST, &url, &json!({ "data": "" })).await?;

	accepted(created).await.map(drop)
}

async fn anonymous(
	services: &Services,
	method: Method,
	url: &str,
	body: &Value,
) -> Result<Response> {
	raw(services, method, url, &body.to_string()).await
}

// A harness item only this binary reached would be dead code in the others.
async fn raw(services: &Services, method: Method, url: &str, body: &str) -> Result<Response> {
	services
		.client
		.clients
		.default
		.request(method, url)
		.header(CONTENT_TYPE, "application/json")
		.body(body.to_owned())
		.send()
		.await
		.map_err(Into::into)
}

async fn assert_refused(response: Response, status: StatusCode, errcode: &str) -> Result {
	assert_eq!(response.status(), status);

	let body: Value = response.json().await?;

	assert_eq!(field(&body, "errcode")?, errcode);

	Ok(())
}
