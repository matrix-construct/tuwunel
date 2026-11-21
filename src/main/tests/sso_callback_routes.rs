#![cfg(test)]

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

mod fixture;

use reqwest::{Client, StatusCode, Url, header::LOCATION, redirect::Policy};
use serde_json::Value;
use tuwunel_core::{Result, utils::html::escape as html_escape};
use tuwunel_service::Services;

use self::fixture::boot;

const IDP: &str = "test-idp";
const ORIGIN: &str = "https://localhost";
const USER_CODE: &str = "BCDF-GHJK";

/// A configured provider's browser-facing routes follow its config.
///
/// Discovery runs on first use, so the provider needs no live issuer.
#[test]
fn provider_routes_follow_the_config() -> Result {
	let option = |key: &str, value: &str| format!("identity_provider.{IDP}.{key}=\"{value}\"");
	let callback = format!("{ORIGIN}/_tuwunel/sso/callback/{IDP}");
	let options = [
		"logging.enable=false",
		&format!("well_known.client=\"{ORIGIN}\""),
		&option("client_id", IDP),
		&option("client_secret", "test-secret"),
		&option("brand", "test"),
		&option("issuer_url", ORIGIN),
		&option("callback_url", &callback),
	];

	boot("sso-callback-routes", options, async |services, base| {
		check_routes(services, base).await?;
		check_device_entry(base).await
	})
}

/// A provider pinned to the new callback path leaves the legacy route unmounted.
///
/// The new route is served regardless, while the legacy path falls through to
/// the unrecognized-endpoint error.
#[tracing::instrument(level = "debug", skip_all)]
async fn check_routes(services: &Services, base: &str) -> Result {
	let client = &services.client.clients.default;
	let legacy = client
		.get(format!("{base}/_matrix/client/unstable/login/sso/callback/{IDP}"))
		.send()
		.await?;

	assert_eq!(legacy.status(), StatusCode::NOT_FOUND, "{legacy:?}");

	let error: Value = legacy.json().await?;

	assert_eq!(error["errcode"], "M_UNRECOGNIZED", "{error}");

	let current = client
		.get(format!("{base}/_tuwunel/sso/callback/{IDP}"))
		.send()
		.await?;

	let status = current.status();
	let body = current.text().await?;

	assert!(!body.contains("M_UNRECOGNIZED"), "{status}: {body}");

	Ok(())
}

/// A code typed into the device entry form reaches the provider through a page.
///
/// Chromium holds every redirect after a form submission to the entry page's
/// `form-action 'self'`, so the form's target answers with a page whose refresh
/// and link lead to the provider's redirect, which returns to the device
/// callback with the code.
#[tracing::instrument(level = "debug", skip_all)]
async fn check_device_entry(base: &str) -> Result {
	let client = Client::builder()
		.redirect(Policy::none())
		.build()?;

	let response = client
		.get(format!("{base}/_tuwunel/oidc/device"))
		.query(&[("user_code", USER_CODE)])
		.send()
		.await?;

	assert_eq!(response.status(), StatusCode::OK, "{response:?}");
	assert!(!response.headers().contains_key(LOCATION), "{response:?}");

	let callback = format!("{ORIGIN}/_tuwunel/oidc/device_callback?user_code={USER_CODE}");
	let sso_redirect = Url::parse_with_params(
		&format!("{ORIGIN}/_matrix/client/v3/login/sso/redirect/{IDP}"),
		[("redirectUrl", callback)],
	)?;

	let href = html_escape(sso_redirect.as_str());
	let html = response.text().await?;

	assert!(html.contains(&format!(r#"content="0; URL={href}""#)), "{html}");
	assert!(html.contains(&format!(r#"href="{href}""#)), "{html}");
	assert!(!html.contains("<form"), "{html}");

	Ok(())
}
