#![cfg(test)]
#![cfg(feature = "livekit")]

//! Native LiveKit token issuance (MSC4195) on a booted server.
//!
//! A stand-in LiveKit records each RoomService call and answers as each case
//! sets it. Tokens go to joined users on both client paths and to this
//! server's own users over federation, every refusal carries its status alone,
//! and an SFU failure answers 502 without a token.

use std::{collections::BTreeMap, net::TcpListener};

use axum::{
	Json, Router,
	body::to_bytes,
	extract::{Request, State},
	http::{Method, StatusCode, header::LOCATION},
	response::{IntoResponse, Response as AxumResponse},
};
use axum_server::from_tcp;
use reqwest::Response;
use serde_json::{Value, from_slice, json};
use serde_urlencoded::{from_str as from_query, to_string as to_query};
use tokio::{
	sync::{
		mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
		watch::{Receiver, Sender, channel},
	},
	task::JoinSet,
};
use tuwunel_core::{
	Result, err,
	http::{HeaderValue, Request as HttpRequest, header::AUTHORIZATION},
	implement,
	ruma::{
		OwnedUserId, RoomId, UserId,
		api::{
			appservice::{Namespace, Namespaces, Registration, RegistrationInit},
			federation::authentication::{XMatrix, XMatrixSigningInput},
		},
		events::room::member::MembershipState,
	},
};
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, register},
	common::{API_KEY, FOLDED_SLOT, SECRET, SLOT, claims, derive, refusal, with},
	fixture::boot,
	state_cache::membership_update,
};

mod client;
mod fixture;

#[path = "livekit/common.rs"]
mod common;

#[path = "fixture/state_cache.rs"]
mod state_cache;

#[expect(
	dead_code,
	reason = "Only the raw GET is used, so each case judges the status."
)]
#[path = "client/get.rs"]
mod get;

const MEMBER_TOKEN: &str = "livekit-member-access-token-00001";
const OUTSIDER_TOKEN: &str = "livekit-outsider-access-token-001";
const MEMBER_ID: &str = "memberABC";

const CREATE_ROOM: &str = "/twirp/livekit.RoomService/CreateRoom";
const REDIRECTED: &str = "/redirected";

// The stand-in's failure text, which must never reach a client.
const STAND_IN_DETAIL: &str = "stand-in failure detail";

const CLIENT_PREFIXES: [&str; 2] = ["v1/", "unstable/io.element.msc4195/"];

const FEDERATION_PATHS: [&str; 2] = [
	"/_matrix/federation/v1/rtc/livekit/get_token",
	"/_matrix/federation/unstable/io.element.msc4195/rtc/livekit/get_token",
];

#[derive(Clone, Copy)]
enum Answer {
	/// The room it was asked for, with this much metadata.
	Room(usize),

	/// A 500 with a Twirp error.
	Failure,

	/// A 302 to a path that answers with the room, were it followed.
	Redirect,
}

#[derive(Clone)]
struct StandIn {
	seen: UnboundedSender<Seen>,
	answer: Receiver<Answer>,
}

struct Callers<'a> {
	member: Client<'a>,
	outsider: Client<'a>,
	guest: Client<'a>,
}

struct Federation<'a> {
	services: &'a Services,
	base: &'a str,
	room_id: &'a RoomId,
	url: &'a str,
}

struct Seen {
	method: Method,
	path: String,
	authorization: String,
	body: Value,
}

/// Runs every case on one server.
///
/// An appservice claims the `rtc/livekit` proxy prefix and points at a closed
/// port, so every token here proves the native route shadows the claim.
#[test]
fn livekit() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let url = format!("ws://{}", listener.local_addr()?);
	let options = [
		format!(r#"livekit.url="{url}""#),
		format!(r#"livekit.api_key="{API_KEY}""#),
		format!(r#"livekit.api_secret="{SECRET}""#),
		r#"well_known.rtc_transports=[{type="org.example.sfu",sfu_url="https://sfu.example.org"}]"#
			.to_owned(),
		r#"well_known.livekit_url="https://livekit.example.org""#.to_owned(),
		"registration.enable=true".to_owned(),
		r#"registration.token="livekit-registration-token""#.to_owned(),
		"registration.guests.enable=true".to_owned(),
	];

	boot(
		"livekit",
		options.iter().map(String::as_str),
		async |services: &Services, base: &str| exercise(services, base, listener, &url).await,
	)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str, listener: TcpListener, url: &str) -> Result {
	listener.set_nonblocking(true)?;

	let (tx, mut seen) = unbounded_channel();
	let (answer, answers) = channel(Answer::Room(0));
	let app = Router::new()
		.fallback(stand_in)
		.with_state(StandIn { seen: tx, answer: answers });

	let serving = from_tcp(listener)?.serve(app.into_make_service());
	let mut tasks = JoinSet::new(); // The set aborts the stand-in when the exercise exits.

	tasks.spawn_on(serving, services.server.runtime());

	let member_user_id = register(services, "lkmember", MEMBER_TOKEN).await?;
	let outsider_user_id = register(services, "lkoutsider", OUTSIDER_TOKEN).await?;
	let guest_token = register_guest(services, base).await?;
	let client = |token| Client { services, base, token };
	let callers = Callers {
		member: client(MEMBER_TOKEN),
		outsider: client(OUTSIDER_TOKEN),
		guest: client(&guest_token),
	};

	let guests_join = json!({
		"preset": "public_chat",
		"initial_state": [{
			"type": "m.room.guest_access",
			"state_key": "",
			"content": { "guest_access": "can_join" },
		}],
	});

	let room_id = callers.member.create_room(&guests_join).await?;

	callers
		.guest
		.post(&format!("rooms/{room_id}/join"), &json!({}))
		.await?;

	register_claim(services).await?;

	let request =
		json!({ "url": url, "room_id": room_id, "slot_id": SLOT, "member_id": MEMBER_ID });

	issues_for_a_joined_user(&callers.member, &member_user_id, &request, &mut seen).await?;
	refuses(services, &callers, &request, &mut seen).await?;
	reports_sfu_failures(&callers.member, &request, &answer, &mut seen).await?;

	let federation = Federation { services, base, room_id: &room_id, url };

	federation_issues(&federation, &member_user_id, &outsider_user_id, &mut seen).await?;
	federation_refuses_alike(&federation, &callers.member, &member_user_id, &outsider_user_id)
		.await?;

	places_native_between(&callers.member, url).await
}

/// A joined user gets a token on both client paths.
///
/// The stand-in sees one CreateRoom per token, for the derived room and with
/// a Bearer token allowed to create rooms. The token verifies with the API
/// secret and admits the user's derived identity to that room; the unstable
/// call slot shares the room of MSC4196's spelling.
#[tracing::instrument(level = "debug", skip_all)]
async fn issues_for_a_joined_user(
	member: &Client<'_>,
	member_user_id: &UserId,
	request: &Value,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	let room_id = field(request, "room_id")?;

	for prefix in CLIENT_PREFIXES {
		let body = accepted(get_token(member, prefix, request).await?).await?;

		admits(field(&body, "jwt")?, member_user_id, room_id)?;
		created_the_room(seen, room_id)?;
		assert!(seen.try_recv().is_err(), "{prefix}: more than one RoomService call");
	}

	Ok(())
}

/// Each refusal answers its HTTP status and Matrix error code before LiveKit
/// is asked anything.
///
/// A user not joined to the room and a remote `server_name` not in it are
/// 403, a `url` other than this server's SFU is 400, and a request without a
/// token is 401. A joined guest gets a token.
#[tracing::instrument(level = "debug", skip_all)]
async fn refuses(
	services: &Services,
	callers: &Callers<'_>,
	request: &Value,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	let elsewhere = with(request, "url", "ws://elsewhere.invalid");
	let remote = with(request, "server_name", "remote.invalid");

	for (client, body, expected) in [
		(&callers.outsider, request, (403, "M_FORBIDDEN")),
		(&callers.member, &elsewhere, (400, "M_INVALID_PARAM")),
		(&callers.member, &remote, (403, "M_FORBIDDEN")),
	] {
		let (status, errcode) = refusal(get_token(client, "v1/", body).await?).await?;

		assert_eq!((status, errcode.as_str()), expected, "{body}");
		assert!(seen.try_recv().is_err(), "{body}: LiveKit was asked");
	}

	let anonymous = services
		.client
		.clients
		.default
		.post(
			callers
				.member
				.client_url("v1/", "rtc/livekit/get_token"),
		)
		.json(request)
		.send()
		.await?;

	assert_eq!(refusal(anonymous).await?, (401, "M_MISSING_TOKEN".into()));

	accepted(get_token(&callers.guest, "v1/", request).await?).await?;
	created_the_room(seen, field(request, "room_id")?)
}

/// An SFU failure answers 502 `M_UNKNOWN` with no token and none of its text.
///
/// A 500 and a 302 both fail; the redirect is not followed, though its target
/// would answer with the room. A CreateRoom answer well past 64 KiB is a
/// success.
#[tracing::instrument(level = "debug", skip_all)]
async fn reports_sfu_failures(
	member: &Client<'_>,
	request: &Value,
	answer: &Sender<Answer>,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	let room_id = field(request, "room_id")?;

	for failure in [Answer::Failure, Answer::Redirect] {
		answer.send_replace(failure);

		let response = get_token(member, "v1/", request).await?;
		let status = response.status();
		let body = response.text().await?;

		assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
		assert!(body.contains("M_UNKNOWN") && !body.contains(STAND_IN_DETAIL), "{body}");
		created_the_room(seen, room_id)?;
		assert!(seen.try_recv().is_err(), "{status}: a second RoomService request");
	}

	answer.send_replace(Answer::Room(80 * 1024));
	accepted(get_token(member, "v1/", request).await?).await?;
	answer.send_replace(Answer::Room(0));

	created_the_room(seen, room_id)
}

/// This server's own user gets a token over federation on both paths.
///
/// A joined user of another server, a user not joined to the room and a
/// `url` other than this server's SFU are refused. The other server's user is
/// joined, so only the check that the user belongs to the origin refuses it.
#[tracing::instrument(level = "debug", skip_all)]
async fn federation_issues(
	federation: &Federation<'_>,
	member_user_id: &UserId,
	outsider_user_id: &UserId,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	let services = federation.services;
	let room_id = federation.room_id.as_str();

	for path in FEDERATION_PATHS {
		let response = federation
			.post(path, member_user_id, room_id, None)
			.await?;

		let body = accepted(response).await?;

		admits(field(&body, "jwt")?, member_user_id, room_id)?;
		created_the_room(seen, room_id)?;
	}

	let foreign = UserId::parse("@lkremote:joined.invalid")?;

	let count = services.globals.next_count();
	let update = membership_update(federation.room_id, &foreign, MembershipState::Join, *count);

	// The count's permit is held until the row lands, as the fixture requires.
	services
		.state_cache
		.update_membership(update)
		.await?;

	drop(count);

	let elsewhere = Some("ws://elsewhere.invalid");

	for (user_id, url, expected) in [
		(&*foreign, None, (403, "M_FORBIDDEN")),
		(outsider_user_id, None, (403, "M_FORBIDDEN")),
		(member_user_id, elsewhere, (400, "M_INVALID_PARAM")),
	] {
		let path = FEDERATION_PATHS[0];
		let response = federation
			.post(path, user_id, room_id, url)
			.await?;

		let (status, errcode) = refusal(response).await?;

		assert_eq!((status, errcode.as_str()), expected, "{user_id} {url:?}");
		assert!(seen.try_recv().is_err(), "{user_id} {url:?}: LiveKit was asked");
	}

	Ok(())
}

/// An origin the room's ACL denies is refused exactly as a user not joined.
///
/// The two 403 bodies are byte for byte the same, so a refusal does not tell
/// the origin which check failed.
#[tracing::instrument(level = "debug", skip_all)]
async fn federation_refuses_alike(
	federation: &Federation<'_>,
	member: &Client<'_>,
	member_user_id: &UserId,
	outsider_user_id: &UserId,
) -> Result {
	let ours = federation.services.globals.server_name();
	let denies_us = json!({
		"initial_state": [{
			"type": "m.room.server_acl",
			"state_key": "",
			"content": { "allow": ["*"], "deny": [ours] },
		}],
	});

	let denied = member.create_room(&denies_us).await?;
	let path = FEDERATION_PATHS[0];
	let not_joined = federation
		.post(path, outsider_user_id, federation.room_id.as_str(), None)
		.await?;

	let acl_denied = federation
		.post(path, member_user_id, denied.as_str(), None)
		.await?;

	assert_eq!(acl_denied.status(), StatusCode::FORBIDDEN);
	assert_eq!(acl_denied.bytes().await?, not_joined.bytes().await?);

	Ok(())
}

/// Both discovery paths list the native transport between the operator's
/// entry and `livekit_url`, and `/versions` advertises the stable endpoints.
///
/// The native entry carries the configured URL and no service URL.
#[tracing::instrument(level = "debug", skip_all)]
async fn places_native_between(member: &Client<'_>, url: &str) -> Result {
	let expected = json!([
		{ "type": "org.example.sfu", "sfu_url": "https://sfu.example.org" },
		{ "type": "m.livekit", "url": url },
		{ "type": "livekit", "livekit_service_url": "https://livekit.example.org" },
	]);

	for (prefix, key) in
		[("v1/", "transports"), ("unstable/org.matrix.msc4143/", "rtc_transports")]
	{
		let transports = member
			.get_url(&member.client_url(prefix, "rtc/transports"))
			.await?;

		let body = accepted(transports).await?;

		assert_eq!(body[key], expected, "{prefix}");
	}

	let versions = member
		.get_url(&member.client_url("", "versions"))
		.await?;

	let features = &accepted(versions).await?["unstable_features"];

	assert_eq!(features["org.matrix.msc4195.stable"], true, "{features}");

	Ok(())
}

/// The stand-in LiveKit: records each request, then answers as the case set.
///
/// A followed redirect lands on its own path and gets the room it names.
async fn stand_in(State(stand_in): State<StandIn>, request: Request) -> AxumResponse {
	let (parts, body) = request.into_parts();
	let bytes = to_bytes(body, usize::MAX)
		.await
		.unwrap_or_default();

	let body: Value = from_slice(&bytes).unwrap_or_default();
	let path = parts.uri.path();

	let query: BTreeMap<String, String> = parts
		.uri
		.query()
		.and_then(|query| from_query(query).ok())
		.unwrap_or_default();

	let name = body["name"]
		.as_str()
		.or_else(|| query.get("name").map(String::as_str))
		.unwrap_or_default()
		.to_owned();

	let answer = match path {
		| REDIRECTED => Answer::Room(0),
		| _ => *stand_in.answer.borrow(),
	};

	let authorization = parts
		.headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.unwrap_or_default()
		.to_owned();

	let seen = Seen {
		path: path.to_owned(),
		method: parts.method,
		authorization,
		body,
	};

	stand_in.seen.send(seen).ok();

	match answer {
		| Answer::Room(metadata) => {
			let room =
				json!({ "sid": "RM_stand_in", "name": name, "metadata": "m".repeat(metadata) });

			Json(room).into_response()
		},
		| Answer::Failure => {
			let error = json!({ "code": "internal", "msg": STAND_IN_DETAIL });

			(StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response()
		},
		| Answer::Redirect => {
			let query = to_query([("name", &name)]).unwrap_or_default();

			(StatusCode::FOUND, [(LOCATION, format!("{REDIRECTED}?{query}"))]).into_response()
		},
	}
}

/// Takes the stand-in's next request and checks it created the call's room.
///
/// It must be a CreateRoom POST for the derived room, carrying a Bearer token
/// signed with the API secret that may create rooms.
fn created_the_room(seen: &mut UnboundedReceiver<Seen>, room_id: &str) -> Result {
	let request = seen
		.try_recv()
		.map_err(|e| err!("the stand-in received nothing: {e}"))?;

	let bearer = request
		.authorization
		.strip_prefix("Bearer ")
		.ok_or_else(|| err!("no Bearer token on {}", request.path))?;

	let grant = &claims(bearer)?["video"];

	assert_eq!((request.method, request.path.as_str()), (Method::POST, CREATE_ROOM));
	assert_eq!(request.body["name"], derive(room_id, FOLDED_SLOT));
	assert_eq!(grant["roomCreate"], true, "{grant}");

	Ok(())
}

/// Checks a token admits the user's member to the call's room.
///
/// The identity and room are derived here by MSC4195's construction, apart
/// from the server's own code, so a token naming the wrong member or room
/// fails.
fn admits(jwt: &str, user_id: &UserId, room_id: &str) -> Result {
	let claims = claims(jwt)?;

	assert_eq!(claims["iss"], API_KEY);
	assert_eq!(claims["sub"], derive(user_id.as_str(), MEMBER_ID));
	assert_eq!(claims["video"]["room"], derive(room_id, FOLDED_SLOT));
	assert_eq!(claims["video"]["roomJoin"], true);

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn get_token(client: &Client<'_>, prefix: &str, body: &Value) -> Result<Response> {
	client
		.post_url(&client.client_url(prefix, "rtc/livekit/get_token"), body)
		.await
}

/// Posts a federation `get_token` request signed as this server.
///
/// The test holds no other server's key, so this server is both origin and
/// destination; the routes treat it as any other origin.
#[implement(Federation, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn post(
	&self,
	path: &str,
	user_id: &UserId,
	room_id: &str,
	url: Option<&str>,
) -> Result<Response> {
	let ours = self.services.globals.server_name();
	let uri = format!("{}{path}", self.base);

	// Keys in sorted order, so the signed body is canonical JSON.
	let body = json!({
		"member_id": MEMBER_ID,
		"room_id": room_id,
		"slot_id": SLOT,
		"url": url.unwrap_or(self.url),
		"user_id": user_id,
	})
	.to_string();

	let request = HttpRequest::builder()
		.method("POST")
		.uri(&uri)
		.body(body.as_str())?;

	let input = XMatrixSigningInput::new(
		ours.to_owned(),
		ours.to_owned(),
		self.services.server_keys.keypair(),
	);

	let signature = XMatrix::sign_http_request(&request, input)
		.map_err(|e| err!("signing the request failed: {e}"))?;

	let response = self
		.services
		.client
		.clients
		.default
		.post(&uri)
		.header(AUTHORIZATION, HeaderValue::from(&signature))
		.header("content-type", "application/json")
		.body(body)
		.send()
		.await?;

	Ok(response)
}

/// Register a guest through the client API and return its access token.
///
/// The account is checked to be a guest, so the guest case runs with a
/// guest's token and not a member's.
#[tracing::instrument(level = "debug", skip_all)]
async fn register_guest(services: &Services, base: &str) -> Result<String> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/register?kind=guest"))
		.json(&json!({}))
		.send()
		.await?;

	let body = accepted(response).await?;
	let user_id: OwnedUserId = field(&body, "user_id")?.try_into()?;

	assert!(services.users.is_guest(&user_id).await, "a guest registration made no guest");

	Ok(field(&body, "access_token")?.to_owned())
}

/// Register an appservice claiming the `rtc/livekit` proxy prefix.
///
/// It forwards to a closed port, so a request it took over would fail.
#[tracing::instrument(level = "debug", skip_all)]
async fn register_claim(services: &Services) -> Result {
	let closed = TcpListener::bind(("127.0.0.1", 0))?.local_addr()?;
	let namespaces = Namespaces {
		users: vec![Namespace::new(false, ".*".to_owned())],
		..Default::default()
	};

	let base: Registration = RegistrationInit {
		id: "lk".to_owned(),
		url: None,
		as_token: "livekit-claim-as-token".to_owned(),
		hs_token: "livekit-claim-hs-token".to_owned(),
		sender_localpart: "lk_bot".to_owned(),
		namespaces,
		rate_limited: None,
		protocols: None,
	}
	.into();

	let registration = Registration {
		proxy_prefix: Some("rtc/livekit".to_owned()),
		proxy_url: Some(format!("http://{closed}")),
		..base
	};

	services
		.appservice
		.register_appservice(registration)
		.await
}
