//! The username availability check against the registration gate.
//!
//! A caller who cannot register a name must learn nothing about one, so while
//! registration is closed to it a taken, a deactivated, a free, an invalid and
//! a forbidden name all get the one refusal `POST /register` gives.

use futures::future::join_all;
use reqwest::{RequestBuilder, Response};
use serde_json::{Value, json};
use tuwunel_core::{Config, Result, itertools::Itertools};
use tuwunel_service::Services;

use super::{
	BRIDGE, BRIDGE_TOKEN, Bridge, Client, OWNER_TOKEN, POOL_CAP,
	appservice::register_appservice_with_exclusivity, boot, register, register_appservice,
};

type Answer = (u16, Value);

const GONE_TOKEN: &str = "appservice-user-join-gone-account-token";
const SHARED_TOKEN: &str = "appservice-user-join-shared-token";

/// An appservice claiming a namespace it does not hold exclusively.
///
/// Its own free names read available to it, where the bridge's read reserved.
const SHARED: Bridge<'static> = Bridge {
	id: "appservice-user-join-shared",
	token: SHARED_TOKEN,
	sender_localpart: "join_shared",
	users: "^@shared_.*:.*$",
	aliases: None,
};

const OPTIONS: [&str; 4] = [
	"logging.enable=false",
	POOL_CAP,
	"registration.forbidden_usernames=[\"^forbidden$\"]",
	"registration.yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse=true",
];

/// An account, a deactivated account, a free name, an invalid name and a
/// forbidden one.
const NAMES: [&str; 5] = ["owner", "gone", "free", "not a name", "forbidden"];

/// Every name gets one refusal while registration is closed to the caller.
///
/// That holds while registration is disabled and then while it is open only to
/// other kinds of login, on both API versions, with no token and with a user's.
/// An appservice's token still gets each name's own answer, a free name in its
/// shared namespace included, and once registration opens an anonymous caller
/// gets a taken name's and a free name's own answers.
#[test]
fn refused_alike_while_registration_is_closed() -> Result {
	boot("appservice-user-join-available", OPTIONS, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "owner", OWNER_TOKEN).await?;

	let gone = register(services, "gone", GONE_TOKEN).await?;

	services.users.deactivate_account(&gone).await?;
	register_appservice(services, &BRIDGE).await?;
	register_appservice_with_exclusivity(services, &SHARED, false).await?;

	let ask = |token, name| available(services, base, "v3", token, name);
	let bridge = Some(BRIDGE_TOKEN);
	let shared = Some(SHARED_TOKEN);
	let free = (200, json!({ "available": true }));
	let in_use = refusal(400, "M_USER_IN_USE", "User ID is not available.");
	let outside = refusal(400, "M_EXCLUSIVE", "Username is not in an appservice namespace.");
	let reserved = refusal(400, "M_EXCLUSIVE", "Username is reserved by an appservice.");

	refused_alike(services, base, "Registration has been disabled.").await?;

	assert_eq!(ask(shared, "shared_free").await?, free, "a free name in a shared namespace");

	// An exclusive namespace answers even its own appservice that a name is reserved.
	assert_eq!(ask(bridge, "join_bot").await?, reserved, "the bridge's own free name");
	assert_eq!(ask(bridge, "free").await?, outside, "a free name outside the bridge");

	Client { services, base, token: BRIDGE_TOKEN }
		.post(
			"register",
			&json!({ "type": "m.login.application_service", "username": "join_bot" }),
		)
		.await?;

	assert_eq!(ask(bridge, "join_bot").await?, in_use, "the bridge's own taken name");

	let original = Config::clone(&services.server.config);

	services
		.config
		.replace(passwordless(opened(original.clone())))?;

	refused_alike(services, base, "Password registration has been disabled.").await?;

	assert_eq!(ask(shared, "shared_free").await?, free, "an appservice, password login off");

	services.config.replace(opened(original))?;

	assert_eq!(ask(None, "owner").await?, in_use, "an account, registration open");
	assert_eq!(ask(None, "free").await?, free, "a free name, registration open");

	Ok(())
}

/// Asserts one refusal carrying `message` for every name.
///
/// Each of `NAMES` gets it on both API versions, with no token and with a
/// user's, and `POST /register` refuses a free name with the same answer.
async fn refused_alike(services: &Services, base: &str, message: &str) -> Result {
	let probes = ["v3", "r0"]
		.into_iter()
		.cartesian_product([None, Some(OWNER_TOKEN)])
		.cartesian_product(NAMES)
		.map(|((version, token), name)| available(services, base, version, token, name));

	let answers: Vec<_> = join_all(probes).await.into_iter().try_collect()?;
	let expected = refusal(403, "M_FORBIDDEN", message);

	assert!(answers.iter().all_equal(), "{message:?} differs by name: {answers:#?}");
	assert_eq!(answers.first(), Some(&expected), "the availability check");
	assert_eq!(register_free(services, base).await?, expected, "POST /register");

	Ok(())
}

async fn available(
	services: &Services,
	base: &str,
	version: &str,
	token: Option<&str>,
	username: &str,
) -> Result<Answer> {
	let url = format!("{base}/_matrix/client/{version}/register/available");
	let request = services
		.client
		.clients
		.default
		.get(url)
		.query(&[("username", username)]);

	let response = token
		.into_iter()
		.fold(request, RequestBuilder::bearer_auth)
		.send()
		.await?;

	answer(response).await
}

async fn register_free(services: &Services, base: &str) -> Result<Answer> {
	let body = json!({ "username": "free", "password": "appservice-user-join-free-password" });
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/register"))
		.json(&body)
		.send()
		.await?;

	answer(response).await
}

async fn answer(response: Response) -> Result<Answer> {
	let status = response.status().as_u16();

	Ok((status, response.json().await?))
}

// The server prefixes an error's message with its code.
fn refusal(status: u16, errcode: &str, message: &str) -> Answer {
	(status, json!({ "errcode": errcode, "error": format!("{errcode}: {message}") }))
}

fn opened(mut config: Config) -> Config {
	config.registration.enable = true;
	config
}

fn passwordless(mut config: Config) -> Config {
	config.authentication.login.with_password = false;
	config
}
