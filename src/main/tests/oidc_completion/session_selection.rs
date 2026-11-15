use reqwest::{Client, Response, StatusCode};
use tuwunel_core::{
	Result,
	ruma::{DeviceId, UserId, device_id},
};
use tuwunel_service::Services;

use super::{account_error, post_form};

const SESSION_END: &str = "org.matrix.session_end";

const SESSIONS_LIST: &str = "org.matrix.sessions_list";

const LOGIN_TOKEN: &str = "session-selection-login";

const REFUSED_TOKEN: &str = "session-selection-refused";

/// Signs out a selection of sessions from the list, sparing the rest.
///
/// The list peeks at its login token and its form carries it, so a selection
/// reaches confirmation without signing in again. Confirmation drops a session
/// already gone, and only the confirming POST signs out and spends the token.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn session_selection(
	services: &Services,
	client: &Client,
	base: &str,
	user: &UserId,
) -> Result {
	for device_id in [device_id!("SELECTED"), device_id!("ALSO_SELECTED"), device_id!("KEPT")] {
		create_session(services, user, device_id).await?;
	}

	let _expires_in = services
		.users
		.create_login_token(user, LOGIN_TOKEN);

	// Every sign-in leg reaches the list carrying an empty `device_id`.
	let list = [("action", SESSIONS_LIST), ("device_id", ""), ("loginToken", LOGIN_TOKEN)];

	let list_page = page(client, base, &list).await?;
	let carried = hidden_fields(&list_page);
	let with = |extra: &[(&'static str, &'static str)]| [carried.as_slice(), extra].concat();

	assert_eq!(carried, [("action", SESSION_END), ("loginToken", LOGIN_TOKEN)]);
	assert!(list_page.contains(r#"name="device_id" value="KEPT""#));

	let response = get_callback(client, base, &carried).await?;

	account_error(response, StatusCode::BAD_REQUEST, "Select at least one session").await?;

	let gone = with(&[("device_id", "SIGNED_OUT_ELSEWHERE")]);
	let response = get_callback(client, base, &gone).await?;

	account_error(response, StatusCode::NOT_FOUND, "Session not found").await?;

	let single = with(&[("device_id", "KEPT")]);
	let single_page = page(client, base, &single).await?;

	assert!(single_page.contains("Sign out 1 session(s)?"));
	assert!(single_page.contains("action=org.matrix.session_view&device_id=KEPT&"));

	let selection = [
		("device_id", "SELECTED"),
		("device_id", "ALSO_SELECTED"),
		("device_id", "SIGNED_OUT_ELSEWHERE"),
	];

	let chosen = with(&selection);
	let confirm_page = page(client, base, &chosen).await?;
	let exists = |device_id| services.users.device_exists(user, device_id);

	assert!(confirm_page.contains("Sign out 2 session(s)?"));
	assert!(!confirm_page.contains("SIGNED_OUT_ELSEWHERE"));
	assert!(!confirm_page.contains("KEPT"));
	assert!(exists(device_id!("SELECTED")).await);

	let outcome = post_form(client, base, "account_callback", &hidden_fields(&confirm_page))
		.await?
		.error_for_status()?
		.text()
		.await?;

	assert!(outcome.contains("Signed out 2 session(s)"));
	assert!(!exists(device_id!("SELECTED")).await);
	assert!(!exists(device_id!("ALSO_SELECTED")).await);
	assert!(exists(device_id!("KEPT")).await);

	let replay = get_callback(client, base, &list).await?;

	account_error(replay, StatusCode::FORBIDDEN, "Invalid or expired login token").await?;

	let repeated = [("action", SESSION_END), ("action", SESSION_END)];
	let response = get_callback(client, base, &repeated).await?;

	account_error(response, StatusCode::BAD_REQUEST, "Invalid account management").await?;

	let response = post_form(client, base, "account_callback", &repeated).await?;

	account_error(response, StatusCode::BAD_REQUEST, "Invalid account management").await?;

	let _expires_in = services
		.users
		.create_login_token(user, REFUSED_TOKEN);

	// A refused sign-out leaves the token for another try.
	let empty = [("action", SESSION_END), ("loginToken", REFUSED_TOKEN)];
	let response = post_form(client, base, "account_callback", &empty).await?;

	account_error(response, StatusCode::BAD_REQUEST, "Select at least one session").await?;

	let gone = [("action", SESSION_END), ("loginToken", REFUSED_TOKEN), ("device_id", "GONE")];
	let response = post_form(client, base, "account_callback", &gone).await?;

	account_error(response, StatusCode::NOT_FOUND, "Session not found").await?;

	let relist = [("action", SESSIONS_LIST), ("loginToken", REFUSED_TOKEN)];

	page(client, base, &relist).await?;
	services
		.users
		.remove_device(user, device_id!("KEPT"))
		.await;

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn create_session(services: &Services, user: &UserId, device_id: &DeviceId) -> Result {
	let token = format!("session-selection-access-token-{device_id}");

	services
		.users
		.create_device(user, Some(device_id), (Some(&token), None), None, None, None)
		.await
		.map(drop)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn page(client: &Client, base: &str, fields: &[(&str, &str)]) -> Result<String> {
	get_callback(client, base, fields)
		.await?
		.error_for_status()?
		.text()
		.await
		.map_err(Into::into)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn get_callback(client: &Client, base: &str, fields: &[(&str, &str)]) -> Result<Response> {
	client
		.get(format!("{base}/_tuwunel/oidc/account_callback"))
		.query(fields)
		.send()
		.await
		.map_err(Into::into)
}

fn hidden_fields(html: &str) -> Vec<(&str, &str)> {
	html.split(r#"<input type="hidden" name=""#)
		.skip(1)
		.filter_map(|field| field.split_once(r#"" value=""#))
		.filter_map(|(name, rest)| {
			rest.split_once('"')
				.map(|(value, _)| (name, value))
		})
		.collect()
}
