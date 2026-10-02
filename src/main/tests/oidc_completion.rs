#![cfg(test)]

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

use std::{net::TcpListener, time::Duration};

use futures::future::{join, try_join};
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

const USERNAME: &str = "oidccompletion";
const PASSWORD: &str = "oidc-completion-test-password";
const REGISTRATION_TOKEN: &str = "oidc-registration-test";
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
		.with_option("allow_registration=true")
		.with_option(format!("registration_token=\"{REGISTRATION_TOKEN}\""))
		.with_option("oidc_native_auth=true")
		.with_option("oidc_require_pkce=false")
		.with_option("oidc_require_client_approval=true")
		.with_option("oidc_rc_per_second=0")
		.with_option("rate_limiting.login.account.burst_count=1000")
		.with_option(
			"oidc_registration_allowed_redirect_hosts=[\"trusted.example\",\"127.0.0.1\"]",
		);

	let args = [("first", "First SSO"), ("second", "Second SSO")]
		.into_iter()
		.fold(args, with_provider);

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

fn with_provider(args: Args, (brand, name): (&str, &str)) -> Args {
	let option = |key: &str, value: &str| format!("identity_provider.{brand}.{key}=\"{value}\"");

	args.with_option(option("client_id", &format!("{brand}-idp")))
		.with_option(option("client_secret", "test-secret"))
		.with_option(option("brand", brand))
		.with_option(option("name", name))
		.with_option(option("issuer_url", &format!("https://{brand}.invalid")))
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let user = UserId::parse_with_server_name(USERNAME, services.globals.server_name())?;

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

	check_provider_choices(services, &client, base).await?;
	check_local_resubmission(services, &client, base).await?;
	check_branch_race(services, &client, base).await?;
	check_registration_claim(services, &client, base).await?;
	concurrent_completion_only_once(services, &client, base).await?;
	stale_refusal_burns_token(services, &client, base).await?;

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

async fn check_provider_choices(services: &Services, client: &Client, base: &str) -> Result {
	let redirect = "https://trusted.example/callback";
	let client_id = register_client(services, redirect).await?;

	for (provider, other_provider) in [("first-idp", "second-idp"), ("second-idp", "first-idp")] {
		let req_id = new_native_request(client, base, &client_id, redirect).await?;
		let html = native_page(client, base, &req_id).await?;

		assert!(html.contains("First SSO"));
		assert!(html.contains("Second SSO"));
		assert!(html.contains(r#"name="password""#));

		let selection = select_provider(client, base, &req_id, provider)
			.await?
			.error_for_status()?;

		let sso_url = location(&selection)?;

		assert!(sso_url.path().ends_with(provider));

		let callback = Url::parse(&parameter(&sso_url, "redirectUrl"))?;
		let selected_req_id = parameter(&callback, "oidc_req_id");

		assert_eq!(selected_req_id, req_id);

		let selected = peek_request(services, &req_id).await?;

		assert_eq!(selected.idp_id.as_deref(), Some(provider));
		assert!(!selected.local_auth_selected);
		assert_eq!(selected.redirect_uri, redirect);

		let repeated = select_provider(client, base, &req_id, provider).await?;

		assert!(
			repeated.status().is_redirection(),
			"repeating the chosen provider still redirects"
		);

		let second_selection = select_provider(client, base, &req_id, other_provider).await?;

		assert!(second_selection.status().is_client_error());

		let password_attempt = submit_password(client, base, &req_id).await?;

		assert_eq!(password_attempt.status(), StatusCode::FORBIDDEN);

		let returned = native_page(client, base, &req_id).await?;

		assert!(returned.contains(&format!("idp_id={provider}")));
		assert!(!returned.contains(&format!("idp_id={other_provider}")));
		assert!(!returned.contains(r#"name="password""#));
	}

	let req_id = new_native_request(client, base, &client_id, redirect).await?;
	let first = select_provider(client, base, &req_id, "first-idp");
	let second = select_provider(client, base, &req_id, "second-idp");
	let (first, second) = try_join(first, second).await?;
	let statuses = [first.status(), second.status()];
	let selected = statuses
		.iter()
		.copied()
		.filter(StatusCode::is_redirection)
		.count();

	assert_eq!(selected, 1, "only one concurrent provider selection may succeed");
	assert!(statuses.iter().any(StatusCode::is_client_error));

	Ok(())
}

async fn register_client(services: &Services, redirect: &str) -> Result<String> {
	let registration = json!({ "redirect_uris": [redirect] });

	services
		.oauth
		.get_server()?
		.register_client(from_value(registration)?)
		.await
		.map(|registration| registration.client_id)
}

async fn new_native_request(
	client: &Client,
	base: &str,
	client_id: &str,
	redirect: &str,
) -> Result<String> {
	let response = client
		.get(format!("{base}/_tuwunel/oidc/authorize"))
		.query(&[
			("client_id", client_id),
			("redirect_uri", redirect),
			("response_type", "code"),
			("scope", "openid"),
		])
		.send()
		.await?
		.error_for_status()?;

	let native = location(&response)?;

	assert_eq!(native.path(), "/_tuwunel/oidc/native");

	Ok(parameter(&native, "oidc_req_id"))
}

async fn native_page(client: &Client, base: &str, req_id: &str) -> Result<String> {
	client
		.get(format!("{base}/_tuwunel/oidc/native"))
		.query(&[("oidc_req_id", req_id)])
		.send()
		.await?
		.error_for_status()?
		.text()
		.await
		.map_err(Into::into)
}

async fn select_provider(
	client: &Client,
	base: &str,
	req_id: &str,
	provider: &str,
) -> Result<Response> {
	client
		.get(format!("{base}/_tuwunel/oidc/native"))
		.query(&[("oidc_req_id", req_id), ("idp_id", provider)])
		.send()
		.await
		.map_err(Into::into)
}

fn location(response: &Response) -> Result<Url> {
	let location = response.headers()["location"]
		.to_str()
		.expect("location header");

	Url::parse(location).map_err(Into::into)
}

async fn peek_request(services: &Services, req_id: &str) -> Result<AuthRequest> {
	services
		.oauth
		.get_server()?
		.peek_auth_request(req_id)
		.await
}

async fn submit_password(client: &Client, base: &str, req_id: &str) -> Result<Response> {
	let fields = [("oidc_req_id", req_id), ("username", USERNAME), ("password", PASSWORD)];

	post_form(client, base, "native", &fields).await
}

async fn post_form(
	client: &Client,
	base: &str,
	route: &str,
	fields: &[(&str, &str)],
) -> Result<Response> {
	client
		.post(format!("{base}/_tuwunel/oidc/{route}"))
		.form(fields)
		.send()
		.await
		.map_err(Into::into)
}

async fn check_local_resubmission(services: &Services, client: &Client, base: &str) -> Result {
	let redirect = "https://trusted.example/resubmission";
	let client_id = register_client(services, redirect).await?;
	let completion = login(client, base, &client_id, redirect, "query").await?;
	let req_id = parameter(&completion, "oidc_req_id");
	let local = peek_request(services, &req_id).await?;

	assert!(local.local_auth_selected);
	assert!(local.idp_id.is_none());

	let resubmitted = submit_password(client, base, &req_id).await?;

	assert_eq!(
		resubmitted.status(),
		StatusCode::SEE_OTHER,
		"a resubmitted local form still completes"
	);

	let late_selection = select_provider(client, base, &req_id, "first-idp").await?;

	assert!(late_selection.status().is_client_error());

	Ok(())
}

async fn check_branch_race(services: &Services, client: &Client, base: &str) -> Result {
	let redirect = "https://trusted.example/race";
	let client_id = register_client(services, redirect).await?;
	let req_id = new_native_request(client, base, &client_id, redirect).await?;
	let password_attempt = submit_password(client, base, &req_id);
	let selection = select_provider(client, base, &req_id, "first-idp");
	let (password_attempt, selection) = try_join(password_attempt, selection).await?;

	assert_ne!(
		password_attempt.status().is_redirection(),
		selection.status().is_redirection(),
		"password and provider branches cannot both succeed",
	);

	Ok(())
}

async fn check_registration_claim(services: &Services, client: &Client, base: &str) -> Result {
	let redirect = "https://trusted.example/registration";
	let client_id = register_client(services, redirect).await?;
	let req_id = new_native_request(client, base, &client_id, redirect).await?;
	let server_name = services.globals.server_name();
	let username = "oidcregistration";
	let user = UserId::parse_with_server_name(username, server_name)?;
	let rejected = submit_registration(client, base, &req_id, username, "invalid-token").await?;

	assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
	assert!(!services.users.exists(&user).await);

	let pending = peek_request(services, &req_id).await?;

	assert!(!pending.local_auth_selected);
	assert!(pending.idp_id.is_none());

	let registered =
		submit_registration(client, base, &req_id, username, REGISTRATION_TOKEN).await?;

	assert_eq!(registered.status(), StatusCode::SEE_OTHER);
	assert!(services.users.exists(&user).await);

	let claimed = peek_request(services, &req_id).await?;

	assert!(claimed.local_auth_selected);
	assert!(claimed.idp_id.is_none());

	let late_selection = select_provider(client, base, &req_id, "first-idp").await?;

	assert!(late_selection.status().is_client_error());

	let race_req_id = new_native_request(client, base, &client_id, redirect).await?;
	let race_username = "oidcregistrationrace";
	let race_user = UserId::parse_with_server_name(race_username, server_name)?;
	let registration =
		submit_registration(client, base, &race_req_id, race_username, REGISTRATION_TOKEN);

	let selection = select_provider(client, base, &race_req_id, "first-idp");
	let (registration, selection) = try_join(registration, selection).await?;

	assert_ne!(registration.status().is_redirection(), selection.status().is_redirection());

	let created = services.users.exists(&race_user).await;

	assert_eq!(
		created,
		registration.status() == StatusCode::SEE_OTHER,
		"a rejected registration must not create an account",
	);

	Ok(())
}

async fn submit_registration(
	client: &Client,
	base: &str,
	req_id: &str,
	username: &str,
	token: &str,
) -> Result<Response> {
	let fields = [
		("oidc_req_id", req_id),
		("mode", "register"),
		("username", username),
		("password", PASSWORD),
		("registration_token", token),
	];

	post_form(client, base, "native", &fields).await
}

async fn concurrent_completion_only_once(
	services: &Services,
	client: &Client,
	base: &str,
) -> Result {
	let redirect = "https://trusted.example/one-shot";
	let client_id = register_client(services, redirect).await?;
	let completion = login(client, base, &client_id, redirect, "query").await?;
	let fetch = || client.get(completion.as_str()).send();
	let (first, second) = try_join(fetch(), fetch()).await?;
	let statuses = [first.status(), second.status()];
	let minted = statuses
		.iter()
		.filter(|status| **status == StatusCode::OK)
		.count();

	assert_eq!(minted, 1, "only one concurrent completion may mint a code");
	assert!(statuses.contains(&StatusCode::NOT_FOUND));

	Ok(())
}

async fn stale_refusal_burns_token(services: &Services, client: &Client, base: &str) -> Result {
	let user = UserId::parse_with_server_name(USERNAME, services.globals.server_name())?;
	let token = "oidc-stale-refusal-token";
	let fields = [("oidc_req_id", "stale"), ("loginToken", token), ("action", "deny")];

	_ = services.users.create_login_token(&user, token);

	let refused = post_form(client, base, "_complete", &fields).await?;
	let burned = services
		.users
		.peek_login_token(token)
		.await
		.is_err();

	assert_eq!(refused.status(), StatusCode::OK);
	assert!(burned, "a refusal burns the login token even when the request is gone");

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
	} else {
		assert!(html.contains("auth-card auth-complete"));
		assert!(html.contains("Finish signing in"));
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

	let native = location(&response)?;
	let req_id = parameter(&native, "oidc_req_id");
	let response = submit_password(client, base, &req_id).await?;

	assert_eq!(response.status(), StatusCode::SEE_OTHER);

	let completion = location(&response)?;

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
	let req_id = parameter(completion, "oidc_req_id");
	let token = parameter(completion, "loginToken");
	let fields = [("oidc_req_id", req_id.as_str()), ("loginToken", &token), ("action", action)];

	post_form(client, base, "_complete", &fields).await
}

fn parameter(url: &Url, name: &str) -> String {
	url.query_pairs()
		.find(|(key, _)| key == name)
		.expect("query parameter")
		.1
		.into_owned()
}
