use std::collections::BTreeMap;

use serde_json::{Map as JsonObject, Value as JsonValue};
use tokio::sync::RwLock;
pub use tuwunel_core::config::IdentityProvider as Provider;
use tuwunel_core::{Err, Result, debug, debug::INFO_SPAN_LEVEL, err, implement};
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

	if provider.issuer_url.is_none() {
		_ = provider
			.issuer_url
			.replace(match provider.brand.as_str() {
				| "github" => "https://github.com".try_into()?,
				| "gitlab" => "https://gitlab.com".try_into()?,
				| "google" => "https://accounts.google.com".try_into()?,
				| _ => return Err!(Config("issuer_url", "Required for this provider.")),
			});
	}

	if provider.base_path.is_none() {
		provider.base_path = match provider.brand.as_str() {
			| "github" => Some("/login/oauth".to_owned()),
			| _ => None,
		};
	}

	let response = self
		.discover(&provider)
		.await
		.and_then(|response| {
			response.as_object().cloned().ok_or_else(|| {
				err!(Request(NotJson("Expecting JSON object for discovery response")))
			})
		})
		.and_then(|response| check_issuer(response, &provider))?;

	if provider.authorization_url.is_none() {
		response
			.get("authorization_endpoint")
			.and_then(JsonValue::as_str)
			.map(Url::parse)
			.transpose()?
			.or_else(|| make_url(&provider, "/authorize").ok())
			.map(|url| provider.authorization_url.replace(url));
	}

	if provider.revocation_url.is_none() {
		response
			.get("revocation_endpoint")
			.and_then(JsonValue::as_str)
			.map(Url::parse)
			.transpose()?
			.or_else(|| make_url(&provider, "/revocation").ok())
			.map(|url| provider.revocation_url.replace(url));
	}

	if provider.introspection_url.is_none() {
		response
			.get("introspection_endpoint")
			.and_then(JsonValue::as_str)
			.map(Url::parse)
			.transpose()?
			.or_else(|| make_url(&provider, "/introspection").ok())
			.map(|url| provider.introspection_url.replace(url));
	}

	if provider.userinfo_url.is_none() {
		response
			.get("userinfo_endpoint")
			.and_then(JsonValue::as_str)
			.map(Url::parse)
			.transpose()?
			.or_else(|| match provider.brand.as_str() {
				| "github" => "https://api.github.com/user".try_into().ok(),
				| _ => make_url(&provider, "/userinfo").ok(),
			})
			.map(|url| provider.userinfo_url.replace(url));
	}

	if provider.token_url.is_none() {
		response
			.get("token_endpoint")
			.and_then(JsonValue::as_str)
			.map(Url::parse)
			.transpose()?
			.or_else(|| {
				let path = if provider.brand == "github" {
					"/access_token"
				} else {
					"/token"
				};

				make_url(&provider, path).ok()
			})
			.map(|url| provider.token_url.replace(url));
	}

	Ok(provider)
}

/// Send a network request to a provider at the computed location of the
/// `.well-known/openid-configuration`, returning the configuration.
#[implement(Providers)]
#[tracing::instrument(level = "debug", ret(level = "trace"), skip(self))]
pub async fn discover(&self, provider: &Provider) -> Result<JsonValue> {
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

/// Compute the location of the `/.well-known/openid-configuration` based on the
/// local provider config.
fn discovery_url(provider: &Provider) -> Result<Url> {
	let default_url = provider
		.discovery
		.then(|| make_url(provider, "/.well-known/openid-configuration"))
		.transpose()?;

	let Some(url) = provider
		.discovery_url
		.clone()
		.filter(|_| provider.discovery)
		.or(default_url)
	else {
		return Err!(Config(
			"discovery_url",
			"Failed to determine URL for discovery of provider {}",
			provider.id()
		));
	};

	Ok(url)
}

/// Validate that the locally configured `issuer_url` matches the issuer claimed
/// in any response. todo: cryptographic validation is not yet implemented here.
fn check_issuer(
	response: JsonObject<String, JsonValue>,
	provider: &Provider,
) -> Result<JsonObject<String, JsonValue>> {
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

	Ok(response)
}

/// Generate a full URL for a request to the idp based on the idp's derived
/// configuration.
fn make_url(provider: &Provider, path: &str) -> Result<Url> {
	let mut suffix = provider.base_path.clone().unwrap_or_default();

	suffix.push_str(path);
	let url = provider
		.issuer_url
		.as_ref()
		.ok_or_else(|| {
			let id = &provider.client_id;
			err!(Config("issuer_url", "Provider {id:?} required field"))
		})?
		.join(&suffix)?;

	Ok(url)
}
