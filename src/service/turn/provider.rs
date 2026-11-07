//! Requests TURN credentials from the configured provider.
//!
//! Answers come in the TURN REST API shape or as WebRTC ICE servers, and both
//! fold into the single username, password and URI list a Matrix client takes.
//! Cloudflare credentials are capped at the 48-hour lifetime Cloudflare allows,
//! and the port 53 URIs it offers, which browsers refuse, are dropped.

#[cfg(test)]
mod tests;

use std::{iter::once, time::Duration};

use reqwest::{Error as HttpError, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Result,
	config::turn::{Cloudflare, Endpoint, Provider, Turn},
	either::{self, Either},
	err, implement,
	utils::resolve_secret,
};
use url::Url;

use super::{Credentials, Service};
use crate::client::read_response_capped;

/// Answer shapes accepted from a credential provider.
///
/// Serde tries the variants in order, so an answer carrying `uris` reads as the
/// TURN REST API and anything else as WebRTC ICE servers.
#[derive(Deserialize)]
#[serde(untagged)]
enum Answer {
	/// TURN REST API, from the expired draft-uberti-behave-turn-rest-00.
	Rest {
		username: String,
		password: String,
		ttl: u64,
		uris: Vec<String>,
	},

	/// WebRTC `RTCConfiguration`, as Cloudflare and STUNner's `/ice` return.
	Configuration {
		#[serde(rename = "iceServers")]
		ice_servers: Vec<IceServer>,
	},

	/// WebRTC `RTCIceServer` entries without the configuration around them.
	Servers(Vec<IceServer>),
}

#[derive(Deserialize)]
struct IceServer {
	/// One URL as a bare string or several as a list, both of which WebRTC allows.
	#[serde(with = "either::serde_untagged")]
	urls: Either<String, Vec<String>>,

	username: Option<String>,
	credential: Option<String>,
}

#[derive(Serialize)]
struct GenerateIceServers {
	ttl: u64,
}

/// Bound on one request to the provider.
///
/// Concurrent callers wait out the same request, so it stays short.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on the provider's answer.
///
/// Real answers are a few hundred bytes; the cap only stops a runaway provider.
const MAX_ANSWER: usize = 64 * 1024;

/// Longest credential lifetime Cloudflare grants.
///
/// Cloudflare refuses longer requests, so the configured lifetime is cut to it.
const CLOUDFLARE_MAX_TTL: Duration = Duration::from_hours(48);

#[implement(Service)]
pub(super) async fn fetch(&self, turn: &Turn, provider: Provider<'_>) -> Result<Credentials> {
	let ttl = Duration::from_secs(turn.ttl);

	match provider {
		| Provider::Endpoint(endpoint, url) => self.fetch_endpoint(endpoint, url, ttl).await,
		| Provider::Cloudflare(cloudflare, key_id) =>
			self.fetch_cloudflare(cloudflare, key_id, ttl.min(CLOUDFLARE_MAX_TTL))
				.await,
	}
}

#[implement(Service)]
async fn fetch_endpoint(
	&self,
	endpoint: &Endpoint,
	url: &Url,
	ttl: Duration,
) -> Result<Credentials> {
	let token = resolve_secret(
		endpoint.token_file.as_deref(),
		endpoint.token.as_deref(),
		"TURN endpoint token",
	);

	let request = self
		.services
		.client
		.default
		.get(with_query(url.clone(), ttl));

	let request = token
		.into_iter()
		.fold(request, RequestBuilder::bearer_auth);

	request_credentials(request, ttl, |_| true).await
}

fn with_query(mut url: Url, ttl: Duration) -> Url {
	url.query_pairs_mut()
		.append_pair("service", "turn")
		.append_pair("ttl", &ttl.as_secs().to_string());

	url
}

#[implement(Service)]
async fn fetch_cloudflare(
	&self,
	cloudflare: &Cloudflare,
	key_id: &str,
	ttl: Duration,
) -> Result<Credentials> {
	let token = resolve_secret(
		cloudflare.token_file.as_deref(),
		cloudflare.token.as_deref(),
		"Cloudflare TURN token",
	)
	.ok_or_else(|| err!(Config("turn.cloudflare.token", "No API token is set.")))?;

	let url = format!(
		"https://rtc.live.cloudflare.com/v1/turn/keys/{key_id}/credentials/generate-ice-servers"
	);

	let request = self
		.services
		.client
		.default
		.post(url)
		.bearer_auth(token)
		.json(&GenerateIceServers { ttl: ttl.as_secs() });

	request_credentials(request, ttl, |uri| !uses_port_53(uri)).await
}

/// Sends the request and converts the provider's answer into credentials.
///
/// The answer must parse as one of the accepted shapes, and only the TURN URIs
/// `keep` accepts remain.
#[tracing::instrument(level = "debug", skip_all)]
async fn request_credentials<Keep>(
	request: RequestBuilder,
	ttl: Duration,
	keep: Keep,
) -> Result<Credentials>
where
	Keep: Fn(&str) -> bool,
{
	// The URL may carry an API key in its query, so errors drop it before logging.
	let response = request
		.timeout(TIMEOUT)
		.send()
		.await
		.and_then(Response::error_for_status)
		.map_err(HttpError::without_url)?;

	let body = read_response_capped(response, MAX_ANSWER).await?;
	let answer: Answer = serde_json::from_slice(&body)?;

	answer.into_credentials(ttl, keep)
}

impl Answer {
	/// Converts the answer into credentials, keeping only the TURN URIs `keep`
	/// accepts.
	///
	/// The spec admits TURN URIs alone, though providers list STUN ones too. A
	/// lifetime longer than the one requested is cut to it, and an answer left
	/// with no URIs or no lifetime is an error.
	fn into_credentials<Keep>(self, requested: Duration, keep: Keep) -> Result<Credentials>
	where
		Keep: Fn(&str) -> bool,
	{
		let keep = |uri: &str| is_turn(uri) && keep(uri);
		let credentials = match self {
			| Self::Configuration { ice_servers: servers } | Self::Servers(servers) =>
				ice_credentials(servers, requested, keep)?,
			| Self::Rest { username, password, ttl, uris } => Credentials {
				username,
				password,
				uris: uris.into_iter().filter(|uri| keep(uri)).collect(),
				ttl: Duration::from_secs(ttl).min(requested),
			},
		};

		if credentials.uris.is_empty() || credentials.ttl.is_zero() {
			return Err!(BadServerResponse("The answer holds no usable TURN URIs or lifetime."));
		}

		Ok(credentials)
	}
}

fn is_turn(uri: &str) -> bool { uri.starts_with("turn:") || uri.starts_with("turns:") }

/// Folds WebRTC ICE servers into the one username and password a Matrix client
/// takes.
///
/// The first entry carrying credentials supplies them, and only the URIs of
/// entries carrying the same credentials and accepted by `keep` remain. An
/// answer with no entry carrying credentials is an error.
fn ice_credentials<Keep>(
	servers: Vec<IceServer>,
	ttl: Duration,
	keep: Keep,
) -> Result<Credentials>
where
	Keep: Fn(&str) -> bool,
{
	let (username, password) = servers
		.iter()
		.find_map(IceServer::credentials)
		.map(|(username, password)| (username.to_owned(), password.to_owned()))
		.ok_or_else(|| err!(BadServerResponse("The answer holds no TURN credentials.")))?;

	let chosen = Some((username.as_str(), password.as_str()));
	let uris = servers
		.into_iter()
		.filter(|server| server.credentials() == chosen)
		.flat_map(|server| server.urls.map_left(once).into_iter())
		.filter(|uri| keep(uri))
		.collect();

	Ok(Credentials { username, password, uris, ttl })
}

impl IceServer {
	#[inline]
	fn credentials(&self) -> Option<(&str, &str)> {
		self.username
			.as_deref()
			.zip(self.credential.as_deref())
			.filter(|(username, password)| !username.is_empty() && !password.is_empty())
	}
}

fn uses_port_53(uri: &str) -> bool {
	uri.split_once(':')
		.and_then(|(_, rest)| rest.split('?').next())
		.and_then(|authority| authority.rsplit_once(':'))
		.is_some_and(|(_, port)| port == "53")
}
