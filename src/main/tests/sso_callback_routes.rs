#![cfg(test)]

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

mod fixture;

use reqwest::StatusCode;
use serde_json::Value;
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::fixture::boot;

const IDP: &str = "test-idp";

/// A provider pinned to the new callback path leaves the legacy route unmounted.
///
/// The new route is served regardless, while the legacy path falls through to
/// the unrecognized-endpoint error. Discovery runs on first use, so the
/// provider needs no live issuer.
#[test]
fn legacy_callback_follows_the_provider_config() -> Result {
	let option = |key: &str, value: &str| format!("identity_provider.{IDP}.{key}=\"{value}\"");
	let callback = format!("https://localhost/_tuwunel/sso/callback/{IDP}");
	let options = [
		"logging.enable=false",
		"well_known.client=\"https://localhost\"",
		&option("client_id", IDP),
		&option("client_secret", "test-secret"),
		&option("brand", "test"),
		&option("issuer_url", "https://localhost"),
		&option("callback_url", &callback),
	];

	boot("sso-callback-routes", options, check_routes)
}

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
