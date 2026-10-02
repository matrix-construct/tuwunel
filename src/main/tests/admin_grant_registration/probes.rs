use std::time::SystemTime;

use reqwest::{Client, RequestBuilder, Response, StatusCode, redirect::Policy};
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{OwnedUserId, UserId},
};
use tuwunel_service::{
	Services,
	oauth::server::{AUTH_REQUEST_LIFETIME, AuthRequest},
	registration_tokens::TokenExpires,
};

use super::{MembershipState, boot, member, register};

const PASSWORD: &str = "registration-test-password";
const TOKEN: &str = "admin-name-one-use";
const REQ_ID: &str = "registration-test";
const OPTIONS: [&str; 7] = [
	"create_admin_room=true",
	"grant_admin_to_first_user=true",
	"log_enable=false",
	"allow_registration=true",
	"yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse=true",
	"well_known.client=\"https://localhost\"",
	"oidc_native_auth=true",
];

#[test]
fn registration_refuses_before_uiaa_and_preserves_token() -> Result {
	boot("admin-name-register", OPTIONS, uiaa)
}

#[test]
fn native_registration_refuses_before_spending_token() -> Result {
	boot("admin-name-native", OPTIONS, native)
}

async fn uiaa(services: &Services, base: &str) -> Result {
	prepare(services).await?;

	let client = Client::new();
	let url = format!("{base}/_matrix/client/v3/register");
	let body = json!({"username": "ghost", "password": PASSWORD});
	let response = post_json(&client, &url, &body, StatusCode::BAD_REQUEST).await?;

	assert_eq!(response["errcode"], "M_USER_IN_USE");
	assert!(response.get("session").is_none());
	unspent(services).await;

	let body = json!({
		"username": "ghost",
		"password": PASSWORD,
		"auth": {"type": "m.login.registration_token", "token": TOKEN},
	});

	let response = post_json(&client, &url, &body, StatusCode::BAD_REQUEST).await?;

	assert_eq!(response["errcode"], "M_USER_IN_USE");
	unspent(services).await;

	let body = json!({"username": "fresh", "password": PASSWORD});
	let response = post_json(&client, &url, &body, StatusCode::UNAUTHORIZED).await?;
	let session = response["session"]
		.as_str()
		.expect("UIAA session");

	let body = json!({
		"username": "fresh",
		"password": PASSWORD,
		"auth": {"type": "m.login.registration_token", "token": TOKEN, "session": session},
	});

	post_json(&client, &url, &body, StatusCode::OK).await?;
	registered(services).await
}

async fn native(services: &Services, base: &str) -> Result {
	prepare(services).await?;
	pending_request(services)?;

	let client = Client::builder()
		.redirect(Policy::none())
		.build()?;

	let url = format!("{base}/_tuwunel/oidc/native");
	let response = post_form(&client, &url, "ghost", StatusCode::BAD_REQUEST).await?;
	let text = response.text().await?;

	assert!(text.contains("User ID is not available."));
	unspent(services).await;
	post_form(&client, &url, "fresh", StatusCode::SEE_OTHER).await?;
	registered(services).await
}

async fn prepare(services: &Services) -> Result {
	let first = local(services, "first")?;

	register(services, &first).await?;

	let ghost = local(services, "ghost")?;

	member(services, &services.globals.server_user, &ghost, MembershipState::Invite).await?;
	member(services, &ghost, &ghost, MembershipState::Join).await?;

	let expires = TokenExpires { max_uses: Some(1), max_age: None };

	services
		.registration_tokens
		.create_token(Some(TOKEN), None, expires)
		.await?;

	Ok(())
}

/// Store the authorization request the native page's submissions claim.
///
/// The page refuses a submission whose request is unknown before it reaches
/// the registration checks under test.
fn pending_request(services: &Services) -> Result {
	let now = SystemTime::now();
	let expires_at = now
		.checked_add(AUTH_REQUEST_LIFETIME)
		.expect("request expiry");

	let request = AuthRequest {
		client_id: "registration-probe".to_owned(),
		redirect_uri: "https://localhost/callback".to_owned(),
		scope: "openid".to_owned(),
		state: None,
		nonce: None,
		code_challenge: None,
		code_challenge_method: None,
		idp_id: None,
		local_auth_selected: false,
		response_mode: None,
		created_at: now,
		expires_at,
	};

	services
		.oauth
		.get_server()?
		.store_auth_request(REQ_ID, &request);

	Ok(())
}

async fn post_json(
	client: &Client,
	url: &str,
	body: &Value,
	status: StatusCode,
) -> Result<Value> {
	let response = send(client.post(url).json(body), status).await?;

	response.json().await.map_err(Into::into)
}

async fn post_form(
	client: &Client,
	url: &str,
	username: &str,
	status: StatusCode,
) -> Result<Response> {
	let form = [
		("oidc_req_id", REQ_ID),
		("mode", "register"),
		("username", username),
		("password", PASSWORD),
		("registration_token", TOKEN),
	];

	send(client.post(url).form(&form), status).await
}

async fn send(request: RequestBuilder, status: StatusCode) -> Result<Response> {
	let response = request.send().await?;

	assert_eq!(response.status(), status);

	Ok(response)
}

async fn unspent(services: &Services) {
	services
		.registration_tokens
		.is_token_valid(TOKEN)
		.await
		.expect("refused name spent registration token");
}

async fn registered(services: &Services) -> Result {
	assert!(
		services
			.registration_tokens
			.is_token_valid(TOKEN)
			.await
			.is_err()
	);

	let fresh = local(services, "fresh")?;

	assert!(services.users.exists(&fresh).await);

	Ok(())
}

fn local(services: &Services, name: &str) -> Result<OwnedUserId> {
	UserId::parse_with_server_name(name, services.globals.server_name()).map_err(Into::into)
}
