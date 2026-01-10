pub mod providers;
pub mod sessions;
pub mod user_info;

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64encode};
use reqwest::{
	Method,
	header::{ACCEPT, CONTENT_TYPE},
};
use ruma::UserId;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tuwunel_core::{
	Err, Result, err, implement,
	utils::{hash::sha256, result::LogErr},
};
use url::Url;

pub use self::{
	providers::Provider,
	sessions::{CODE_VERIFIER_LENGTH, SESSION_ID_LENGTH, Session},
	user_info::UserInfo,
};
use self::{providers::Providers, sessions::Sessions};
use crate::SelfServices;

pub struct Service {
	services: SelfServices,
	pub providers: Arc<Providers>,
	pub sessions: Arc<Sessions>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let providers = Arc::new(Providers::build(args));
		let sessions = Arc::new(Sessions::build(args, providers.clone()));
		Ok(Arc::new(Self {
			services: args.services.clone(),
			sessions,
			providers,
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all, ret)]
pub async fn request_userinfo(
	&self,
	(provider, session): (&Provider, &Session),
) -> Result<UserInfo> {
	#[derive(Debug, Serialize)]
	struct Query;

	let url = provider
		.userinfo_url
		.clone()
		.ok_or_else(|| err!(Config("userinfo_url", "Missing userinfo URL in config")))?;

	self.request((Some(provider), Some(session)), Method::GET, url, Option::<Query>::None)
		.await
		.and_then(|value| serde_json::from_value(value).map_err(Into::into))
		.log_err()
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all, ret)]
pub async fn request_tokeninfo(
	&self,
	(provider, session): (&Provider, &Session),
) -> Result<UserInfo> {
	#[derive(Debug, Serialize)]
	struct Query;

	let url = provider
		.introspection_url
		.clone()
		.ok_or_else(|| {
			err!(Config("introspection_url", "Missing introspection URL in config"))
		})?;

	self.request((Some(provider), Some(session)), Method::GET, url, Option::<Query>::None)
		.await
		.and_then(|value| serde_json::from_value(value).map_err(Into::into))
		.log_err()
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all, ret)]
pub async fn revoke_token(&self, (provider, session): (&Provider, &Session)) -> Result {
	#[derive(Debug, Serialize)]
	struct RevokeQuery<'a> {
		client_id: &'a str,
		client_secret: &'a str,
	}

	let client_secret = provider.get_client_secret().await?;

	let query = RevokeQuery {
		client_id: &provider.client_id,
		client_secret: &client_secret,
	};

	let url = provider
		.revocation_url
		.clone()
		.ok_or_else(|| err!(Config("revocation_url", "Missing revocation URL in config")))?;

	self.request((Some(provider), Some(session)), Method::POST, url, Some(query))
		.await
		.log_err()
		.map(|_| ())
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all, ret)]
pub async fn request_token(
	&self,
	(provider, session): (&Provider, &Session),
	code: &str,
) -> Result<Session> {
	#[derive(Debug, Serialize)]
	struct TokenQuery<'a> {
		client_id: &'a str,
		client_secret: &'a str,
		grant_type: &'a str,
		code: &'a str,
		code_verifier: Option<&'a str>,
		redirect_uri: Option<&'a str>,
	}

	let client_secret = provider.get_client_secret().await?;

	let query = TokenQuery {
		client_id: &provider.client_id,
		client_secret: &client_secret,
		grant_type: "authorization_code",
		code,
		code_verifier: session.code_verifier.as_deref(),
		redirect_uri: provider.callback_url.as_ref().map(Url::as_str),
	};

	let url = provider
		.token_url
		.clone()
		.ok_or_else(|| err!(Config("token_url", "Missing token URL in config")))?;

	self.request((Some(provider), Some(session)), Method::POST, url, Some(query))
		.await
		.and_then(|value| serde_json::from_value(value).map_err(Into::into))
		.log_err()
}

/// Send a request to a provider; this is somewhat abstract since URL's are
/// formed prior to this call and could point at anything, however this function
/// uses the oauth-specific http client and is configured for JSON with special
/// casing for an `error` property in the response.
#[implement(Service)]
#[tracing::instrument(
	name = "request",
	level = "debug",
	ret(level = "trace"),
	skip(self, body)
)]
pub async fn request<Body>(
	&self,
	(provider, session): (Option<&Provider>, Option<&Session>),
	method: Method,
	url: Url,
	body: Option<Body>,
) -> Result<JsonValue>
where
	Body: Serialize,
{
	let mut request = self
		.services
		.client
		.oauth
		.request(method, url)
		.header(ACCEPT, "application/json");

	if let Some(body) = body.map(serde_html_form::to_string).transpose()? {
		request = request
			.header(CONTENT_TYPE, "application/x-www-form-urlencoded")
			.body(body);
	}

	if let Some(session) = session {
		if let Some(access_token) = session.access_token.clone() {
			request = request.bearer_auth(access_token);
		}
	}

	let response: JsonValue = request
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	if let Some(response) = response.as_object().as_ref()
		&& let Some(error) = response.get("error").and_then(JsonValue::as_str)
	{
		let description = response
			.get("error_description")
			.and_then(JsonValue::as_str)
			.unwrap_or("(no description)");

		return Err!(Request(Forbidden("Error from provider: {error}: {description}",)));
	}

	Ok(response)
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn get_user(&self, user_id: &UserId) -> Result<(Provider, Session)> {
	let session = self.sessions.get_by_user(user_id).await?;
	let provider = self.sessions.provider(&session).await?;

	Ok((provider, session))
}

#[inline]
pub fn unique_id((provider, session): (&Provider, &Session)) -> Result<String> {
	unique_id_parts((provider, session)).and_then(unique_id_iss_sub)
}

#[inline]
pub fn unique_id_sub((provider, sub): (&Provider, &str)) -> Result<String> {
	unique_id_sub_parts((provider, sub)).and_then(unique_id_iss_sub)
}

#[inline]
pub fn unique_id_iss((iss, session): (&str, &Session)) -> Result<String> {
	unique_id_iss_parts((iss, session)).and_then(unique_id_iss_sub)
}

pub fn unique_id_iss_sub((iss, sub): (&str, &str)) -> Result<String> {
	let hash = sha256::delimited([iss, sub].iter());
	let b64 = b64encode.encode(hash);

	Ok(b64)
}

fn unique_id_parts<'a>(
	(provider, session): (&'a Provider, &'a Session),
) -> Result<(&'a str, &'a str)> {
	provider
		.issuer_url
		.as_ref()
		.map(Url::as_str)
		.ok_or_else(|| err!(Config("issuer_url", "issuer_url not found for this provider.")))
		.and_then(|iss| unique_id_iss_parts((iss, session)))
}

fn unique_id_sub_parts<'a>(
	(provider, sub): (&'a Provider, &'a str),
) -> Result<(&'a str, &'a str)> {
	provider
		.issuer_url
		.as_ref()
		.map(Url::as_str)
		.ok_or_else(|| err!(Config("issuer_url", "issuer_url not found for this provider.")))
		.map(|iss| (iss, sub))
}

fn unique_id_iss_parts<'a>((iss, session): (&'a str, &'a Session)) -> Result<(&'a str, &'a str)> {
	session
		.user_info
		.as_ref()
		.map(|user_info| user_info.sub.as_str())
		.ok_or_else(|| err!(Request(NotFound("user_info not found for this session."))))
		.map(|sub| (iss, sub))
}
