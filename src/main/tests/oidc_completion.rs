#![cfg(test)]

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

use std::{net::TcpListener, time::Duration};

use futures::future::join;
use reqwest::{Client, Response, StatusCode, Url, redirect::Policy};
use serde_json::{from_value, json};
use serde_urlencoded::from_str;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Result, ruma::UserId};
use tuwunel_service::{Services, oauth::server::AuthRequest, users::Register};

use self::client::wait_until_ready;

struct Case<'a> {
	redirect: &'a str,
	native: bool,
	waived: bool,
	automatic: bool,
}

const PASSWORD: &str = "oidc-completion-test-password";
const STATE: &str = "state&<quoted>\"'";

#[test]
fn native_completion_ends_form_navigation() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("well_known.client=\"https://localhost\"")
		.with_option("oidc_native_auth=true")
		.with_option("oidc_require_pkce=false")
		.with_option("oidc_require_client_approval=true")
		.with_option("oidc_rc_per_second=0")
		.with_option(
			"oidc_registration_allowed_redirect_hosts=[\"trusted.example\",\"127.0.0.1\"]",
		);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run?;

		outcome
	})
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let user = UserId::parse_with_server_name("oidccompletion", services.globals.server_name())?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user),
			password: Some(PASSWORD),
			..Default::default()
		})
		.await?;

	let client = Client::builder()
		.redirect(Policy::none())
		.timeout(Duration::from_secs(10))
		.build()?;

	for (redirect, native, waived, automatic) in [
		("https://trusted.example/callback?existing=a%26b", false, true, true),
		("https://untrusted.example/callback?existing=a%26b", false, false, true),
		("http://127.0.0.1:49152/callback?existing=a%26b", true, true, true),
		("https://untrusted.example/universal", true, false, false),
		("io.example.app:/callback", true, false, false),
	] {
		for mode in ["query", "fragment"] {
			let case = Case { redirect, native, waived, automatic };

			complete(services, &client, base, case, mode, &user).await?;
		}
	}

	for action in ["deny", "Approve", ""] {
		let redirect = "https://untrusted.example/denied";
		let registration = json!({ "redirect_uris": [redirect], "application_type": "web" });
		let oidc = services.oauth.get_server()?;
		let registration = oidc
			.register_client(from_value(registration)?)
			.await?;

		let completion = login(&client, base, &registration.client_id, redirect, "query").await?;
		let response = approve(&client, base, &completion, action).await?;

		assert_eq!(response.status(), StatusCode::OK);
		assert!(!response.headers().contains_key("location"));

		let html = response.text().await?;

		assert!(!html.contains("http-equiv=\"refresh\""));
		assert!(!html.contains(redirect));
		assert_eq!(
			client
				.get(completion.as_str())
				.send()
				.await?
				.status(),
			StatusCode::NOT_FOUND
		);

		services
			.users
			.find_from_login_token(&parameter(&completion, "loginToken"))
			.await
			.expect_err("refused login token was consumed");
	}

	for redirect in
		["https://trusted.example/unavailable", "javascript:alert(1)", "data:text/html,x"]
	{
		let original = "https://trusted.example/unavailable";
		let registration = json!({ "redirect_uris": [original], "application_type": "web" });
		let oidc = services.oauth.get_server()?;
		let registration = oidc
			.register_client(from_value(registration)?)
			.await?;

		let completion = login(&client, base, &registration.client_id, original, "query").await?;
		let req_id = parameter(&completion, "oidc_req_id");
		let request = oidc.peek_auth_request(&req_id).await?;
		let request = AuthRequest {
			client_id: "missing-client-registration".to_owned(),
			redirect_uri: redirect.to_owned(),
			..request
		};

		oidc.store_auth_request(&req_id, &request);

		let response = approve(&client, base, &completion, "approve").await?;

		assert!(response.status().is_client_error() || response.status().is_server_error());
		assert!(!response.headers().contains_key("location"));
		assert!(
			!response
				.text()
				.await?
				.contains("http-equiv=\"refresh\"")
		);

		oidc.peek_auth_request(&req_id)
			.await
			.expect("request survives failed completion");

		assert_eq!(
			services
				.users
				.find_from_login_token(&parameter(&completion, "loginToken"))
				.await?,
			user
		);
	}

	Ok(())
}

async fn complete(
	services: &Services,
	client: &Client,
	base: &str,
	Case { redirect, native, waived, automatic }: Case<'_>,
	mode: &str,
	user: &UserId,
) -> Result {
	let registered_redirect = match redirect {
		| "http://127.0.0.1:49152/callback?existing=a%26b" =>
			"http://127.0.0.1/callback?existing=a%26b",
		| _ => redirect,
	};

	let registration = json!({
		"redirect_uris": [registered_redirect],
		"application_type": if native { "native" } else { "web" },
		"client_name": "{redirect}<script>bad()</script>",
		"client_uri": "https://metadata.invalid/not-a-callback",
	});

	let oidc = services.oauth.get_server()?;
	let registration = oidc
		.register_client(from_value(registration)?)
		.await?;

	let client_id = &registration.client_id;
	let completion = login(client, base, client_id, redirect, mode).await?;
	let response = client.get(completion.as_str()).send().await?;
	let response = if waived {
		response
	} else {
		let html = response.error_for_status()?.text().await?;

		assert!(html.contains("value=\"approve\""));
		assert!(!html.contains("http-equiv=\"refresh\""));

		approve(client, base, &completion, "approve").await?
	};

	assert_eq!(response.status(), StatusCode::OK);
	assert!(!response.headers().contains_key("location"));
	assert_eq!(response.headers()["cache-control"], "no-store");
	assert_eq!(response.headers()["referrer-policy"], "no-referrer");
	assert!(
		response.headers()["content-security-policy"]
			.to_str()
			.expect("CSP header text")
			.contains("form-action 'self'")
	);

	let html = response.text().await?;

	assert_eq!(html.contains("http-equiv=\"refresh\""), automatic);
	assert!(!html.contains("<script"));
	assert!(!html.contains(PASSWORD));
	assert!(!html.contains("metadata.invalid"));

	let destination = html
		.split("href=\"")
		.find_map(|part| {
			let candidate = part.split('"').next()?;

			candidate
				.starts_with(redirect.split('?').next()?)
				.then_some(candidate)
		})
		.expect("callback link");

	if automatic {
		assert!(html.contains(&format!("content=\"0; URL={destination}\"")));
		assert!(!html.contains("stylesheet"));
	}

	let destination = Url::parse(&destination.replace("&amp;", "&"))?;
	let registered = Url::parse(redirect)?;

	assert_eq!(destination.scheme(), registered.scheme());
	assert_eq!(destination.host_str(), registered.host_str());
	assert_eq!(destination.port(), registered.port());
	assert_eq!(destination.path(), registered.path());

	match mode {
		| "fragment" => assert_eq!(destination.query(), registered.query()),
		| _ if registered.query().is_some() => assert!(
			destination
				.query_pairs()
				.any(|(key, value)| key == "existing" && value == "a&b")
		),
		| _ => {},
	}

	let pairs = match mode {
		| "fragment" => destination.fragment().expect("fragment response"),
		| _ => destination.query().expect("query response"),
	};

	let pairs: Vec<(String, String)> = from_str(pairs)?;
	let code = pairs
		.iter()
		.find(|(key, _)| key == "code")
		.expect("code")
		.1
		.as_str();

	assert!(
		pairs
			.iter()
			.any(|(key, value)| key == "state" && value == STATE)
	);

	assert!(!html.contains(&parameter(&completion, "loginToken")));

	let session = oidc
		.exchange_auth_code(code, client_id, redirect, None, false)
		.await?;

	assert_eq!(session.user_id.as_str(), user.as_str());
	oidc.exchange_auth_code(code, client_id, redirect, None, false)
		.await
		.expect_err("authorization code is single use");

	assert_eq!(
		client
			.get(completion.as_str())
			.send()
			.await?
			.status(),
		StatusCode::NOT_FOUND
	);

	services
		.users
		.find_from_login_token(&parameter(&completion, "loginToken"))
		.await
		.expect_err("completed login token was consumed");

	Ok(())
}

async fn login(
	client: &Client,
	base: &str,
	client_id: &str,
	redirect: &str,
	mode: &str,
) -> Result<Url> {
	let response = client
		.get(format!("{base}/_tuwunel/oidc/authorize"))
		.query(&[
			("client_id", client_id),
			("redirect_uri", redirect),
			("response_type", "code"),
			("scope", "openid urn:matrix:org.matrix.msc2967.client:api:*"),
			("state", STATE),
			("response_mode", mode),
		])
		.send()
		.await?
		.error_for_status()?;

	let native = Url::parse(
		response.headers()["location"]
			.to_str()
			.expect("native location"),
	)?;

	let req_id = parameter(&native, "oidc_req_id");
	let response = client
		.post(format!("{base}/_tuwunel/oidc/native"))
		.form(&[
			("oidc_req_id", req_id.as_str()),
			("username", "oidccompletion"),
			("password", PASSWORD),
		])
		.send()
		.await?;

	assert_eq!(response.status(), StatusCode::SEE_OTHER);

	let completion = Url::parse(
		response.headers()["location"]
			.to_str()
			.expect("completion location"),
	)?;

	let completion = Url::parse(&format!(
		"{base}{}?{}",
		completion.path(),
		completion.query().expect("completion query")
	))?;

	Ok(completion)
}

async fn approve(
	client: &Client,
	base: &str,
	completion: &Url,
	action: &str,
) -> Result<Response> {
	let fields = [
		("oidc_req_id", parameter(completion, "oidc_req_id")),
		("loginToken", parameter(completion, "loginToken")),
		("action", action.to_owned()),
	];

	let response = client
		.post(format!("{base}/_tuwunel/oidc/_complete"))
		.form(&fields)
		.send()
		.await?;

	Ok(response)
}

fn parameter(url: &Url, name: &str) -> String {
	url.query_pairs()
		.find(|(key, _)| key == name)
		.expect("query parameter")
		.1
		.into_owned()
}
