use std::collections::BTreeMap;

use serde_json::{Map as JsonObject, Value as JsonValue};
use tokio::sync::RwLock;
pub use tuwunel_core::config::IdentityProvider as Provider;
use tuwunel_core::{Err, Result, debug, debug::INFO_SPAN_LEVEL, implement};
use url::Url;

use crate::SelfServices;

/// Discovered providers
#[derive(Default)]
pub struct Providers {
	services: SelfServices,
	providers: RwLock<BTreeMap<ProviderId, Provider>>,
}

/// Identity Provider ID
pub type ProviderId = String;

#[implement(Providers)]
pub(super) fn build(args: &crate::Args<'_>) -> Self {
	Self {
		services: args.services.clone(),
		..Default::default()
	}
}

/// Get the Provider configuration after any discovery and adjustments
/// made on top of the admin's configuration. This incurs network-based
/// discovery on the first call but responds from cache on subsequent calls.
#[implement(Providers)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn get(&self, id: &str) -> Result<Provider> {
	if let Some(provider) = self.get_cached(id).await {
		return Ok(provider);
	}

	let config = self.get_config(id)?;
	let id = config.id().to_owned();
	let mut map = self.providers.write().await;
	let provider = self.configure(config).await?;

	debug!(?id, ?provider);
	_ = map.insert(id, provider.clone());

	Ok(provider)
}

/// Get the admin-configured Provider which exists prior to any
/// reconciliation with the well-known discovery (the server's config is
/// immutable); though it is important to note the server config can be
/// reloaded. This will Err NotFound for a non-existent idp.
///
/// When no provider is found with a matching client_id, providers are then
/// searched by brand. Brand matching will be invalidated when more than one
/// provider matches the brand.
#[implement(Providers)]
pub fn get_config(&self, id: &str) -> Result<Provider> {
	let providers = &self.services.config.identity_provider;

	if let Some(provider) = providers
		.values()
		.find(|config| config.id() == id)
		.cloned()
	{
		return Ok(provider);
	}

	if let Some(provider) = providers
		.values()
		.find(|config| config.brand == id.to_lowercase())
		.filter(|_| {
			providers
				.values()
				.filter(|config| config.brand == id.to_lowercase())
				.count()
				.eq(&1)
		})
		.cloned()
	{
		return Ok(provider);
	}

	Err!(Request(NotFound("Unrecognized Identity Provider")))
}

/// Get the discovered provider from the runtime cache. ID may be client_id or
/// brand if brand is unique among provider configurations.
#[implement(Providers)]
async fn get_cached(&self, id: &str) -> Option<Provider> {
	let providers = self.providers.read().await;

	if let Some(provider) = providers.get(id).cloned() {
		return Some(provider);
	}

	providers
		.values()
		.find(|provider| provider.brand == id.to_lowercase())
		.filter(|_| {
			providers
				.values()
				.filter(|provider| provider.brand == id.to_lowercase())
				.count()
				.eq(&1)
		})
		.cloned()
}

/// Configure an identity provider; takes the admin-configured instance from the
/// server's config, queries the provider for discovery, and then returns an
/// updated config based on the proper reconciliation. This final config is then
/// cached in memory to avoid repeating this process.
#[implement(Providers)]
#[tracing::instrument(
	level = INFO_SPAN_LEVEL,
	ret(level = "debug"),
	skip(self),
)]
async fn configure(&self, mut provider: Provider) -> Result<Provider> {
	_ = provider
		.name
		.get_or_insert_with(|| provider.brand.clone());

	if provider.brand == "github" {
		configure_github(&mut provider);
		return Ok(provider);
	}

	if provider.issuer_url.is_none() {
		provider.issuer_url = Some(match provider.brand.as_str() {
			| "gitlab" => "https://gitlab.com".try_into()?,
			| "google" => "https://accounts.google.com".try_into()?,
			| _ => return Err!(Config("issuer_url", "Required for this provider.")),
		});
	}

	if !provider.discovery {
		assert_manual_urls(&provider)?;
		return Ok(provider);
	}

	let discovery_response = {
		let response = self.discover(&provider).await?;
		let Some(response_map) = response.as_object() else {
			return Err!(Request(NotJson("Expecting JSON object for discovery response")));
		};

		check_issuer(response_map, &provider)?;

		response_map.to_owned()
	};

	if provider.authorization_url.is_none() {
		provider.authorization_url = Some(assert_and_parse_url(
			provider.id(),
			&discovery_response,
			"authorization_endpoint",
		)?);
	}

	if provider.revocation_url.is_none() {
		provider.revocation_url = Some(assert_and_parse_url(
			provider.id(),
			&discovery_response,
			"revocation_endpoint",
		)?);
	}

	if provider.introspection_url.is_none() {
		provider.introspection_url = Some(assert_and_parse_url(
			provider.id(),
			&discovery_response,
			"introspection_endpoint",
		)?);
	}

	if provider.userinfo_url.is_none() {
		provider.userinfo_url =
			Some(assert_and_parse_url(provider.id(), &discovery_response, "userinfo_endpoint")?);
	}

	if provider.token_url.is_none() {
		provider.token_url =
			Some(assert_and_parse_url(provider.id(), &discovery_response, "token_endpoint")?);
	}

	Ok(provider)
}

fn configure_github(provider: &mut Provider) {
	let issuer = "https://github.com"
		.parse::<Url>()
		.expect("valid url");

	let oauth_base = "https://api.github.com/login/oauth/"
		.parse::<Url>()
		.expect("valid url");

	provider.discovery_url = Some(
		issuer
			.join("login/oauth/.well-known/openid-configuration")
			.expect("valid url"),
	);

	provider.issuer_url = Some(issuer);

	provider.authorization_url = Some(oauth_base.join("authorize").expect("valid url"));
	provider.revocation_url = Some(oauth_base.join("revocation").expect("valid url"));
	provider.introspection_url = Some(
		oauth_base
			.join("introspection")
			.expect("valid url"),
	);
	provider.token_url = Some(
		oauth_base
			.join("access_token")
			.expect("valid url"),
	);

	// NOTE: this one doesn't have the '/login/oauth' base
	provider.userinfo_url = Some(
		"https://api.github.com/user"
			.parse::<Url>()
			.expect("valid url"),
	);
}

fn assert_manual_urls(provider: &Provider) -> Result<()> {
	if provider.authorization_url.is_none() {
		return Err!(Config(
			"authorization_url",
			"Required for provider {}, since discovery is disabled",
			provider.client_id
		));
	}

	if provider.revocation_url.is_none() {
		return Err!(Config(
			"revocation_url",
			"Required for provider {}, since discovery is disabled",
			provider.client_id
		));
	}

	if provider.introspection_url.is_none() {
		return Err!(Config(
			"introspection_url",
			"Required for provider {}, since discovery is disabled",
			provider.client_id
		));
	}

	if provider.userinfo_url.is_none() {
		return Err!(Config(
			"userinfo_url",
			"Required for provider {}, since discovery is disabled",
			provider.client_id
		));
	}

	if provider.token_url.is_none() {
		return Err!(Config(
			"token_url",
			"Required for provider {}, since discovery is disabled",
			provider.client_id
		));
	}

	Ok(())
}

/// Send a network request to a provider at the computed location of the
/// `.well-known/openid-configuration`, returning the configuration.
#[implement(Providers)]
#[tracing::instrument(level = "debug", ret(level = "trace"), skip(self))]
async fn discover(&self, provider: &Provider) -> Result<JsonValue> {
	self.services
		.client
		.oauth
		.get(discovery_url(provider)?)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await
		.map_err(Into::into)
}

/// Get the location of the `/.well-known/openid-configuration` based on the
/// local provider config.
fn discovery_url(provider: &Provider) -> Result<Url> {
	if let Some(url) = &provider.discovery_url {
		return Ok(url.to_owned());
	}

	let issuer = provider
		.issuer_url
		.as_ref()
		.expect("issuer to be asserted before calling discover");

	let issuer_path = issuer.path();

	let base_url = if issuer_path.ends_with('/') {
		issuer.to_owned()
	} else {
		let mut url = issuer.to_owned();
		url.set_path((issuer_path.to_owned() + "/").as_str());
		url
	};

	Ok(base_url.join(".well-known/openid-configuration")?)
}

/// Validate that the locally configured `issuer_url` matches the issuer claimed
/// in any response. todo: cryptographic validation is not yet implemented here.
fn check_issuer(response: &JsonObject<String, JsonValue>, provider: &Provider) -> Result<()> {
	let expected = provider
		.issuer_url
		.as_ref()
		.map(Url::as_str)
		.map(|url| url.trim_end_matches('/'));

	let responded = response
		.get("issuer")
		.and_then(JsonValue::as_str)
		.map(|url| url.trim_end_matches('/'));

	if expected != responded {
		return Err!(Request(Unauthorized(
			"Configured issuer_url {expected:?} does not match discovered {responded:?}",
		)));
	}

	Ok(())
}

/// Assert that a url exists in the response and parse it
fn assert_and_parse_url(
	provider_id: &str,
	response_map: &serde_json::Map<String, serde_json::Value>,
	url_name: &str,
) -> Result<Url> {
	let Some(url_value) = response_map.get(url_name) else {
		return Err!(
			"Error building oidc provider '{provider_id}': {url_name} is missing from openid \
			 discovery response",
		);
	};

	Ok(url_value.as_str().unwrap_or_default().parse()?)
}
