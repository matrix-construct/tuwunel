use std::{fmt::Debug, mem, time::Duration};

use bytes::Bytes;
use ipaddress::IPAddress;
use reqwest::{Client, Method, Request, Response, Url};
use ruma::{
	ServerName,
	api::{
		EndpointError, IncomingResponse, MatrixVersion, OutgoingRequest, SupportedVersions,
		error::{Error as RumaError, ErrorBody},
	},
};
use tokio::time::timeout;
use tuwunel_core::{
	Err, Error, Result, debug, debug::INFO_SPAN_LEVEL, debug_error, debug_warn, err, implement,
	trace,
};

use super::{
	ShouldAttempt,
	peer::classify_error,
	scheme::{FedAuth, FedPath},
};
use crate::{client::read_response_capped, resolver::actual::ActualDest};

/// Sends a request to a federation server
#[implement(super::Service)]
#[tracing::instrument(skip_all, name = "request", level = "debug")]
pub async fn execute<T>(&self, dest: &ServerName, request: T) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Debug + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	let client = &self.services.client.federation;
	self.execute_on(client, dest, request).await
}

/// Client-initiated key lookup (`/keys/query`, `/keys/claim`) over federation:
/// skips servers already in backoff and bounds the request by
/// `federation_keys_timeout` so a waiting client is not held past its own send
/// deadline. Honors peer-status but does not record into it; a slow key lookup
/// must not suppress unrelated outbound traffic to the server.
#[implement(super::Service)]
#[tracing::instrument(skip_all, name = "keys", level = "debug")]
pub async fn execute_keys<T>(&self, dest: &ServerName, request: T) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Debug + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	if matches!(self.should_attempt(dest).await, ShouldAttempt::No { .. }) {
		return Err!("{dest} is in federation backoff; skipping key lookup");
	}

	let timeout_dur = Duration::from_secs(
		self.services
			.server
			.config
			.federation_keys_timeout,
	);

	let client = &self.services.client.federation;

	match timeout(timeout_dur, self.execute_uncounted(client, dest, request)).await {
		| Ok(result) => result,
		| Err(_elapsed) => Err!("{dest} key lookup exceeded {}s", timeout_dur.as_secs()),
	}
}

/// Like execute() but with a very large timeout
#[implement(super::Service)]
#[tracing::instrument(skip_all, name = "synapse", level = "debug")]
pub async fn execute_synapse<T>(
	&self,
	dest: &ServerName,
	request: T,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Debug + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	let client = &self.services.client.synapse;
	self.execute_on(client, dest, request).await
}

#[implement(super::Service)]
pub async fn execute_on<T>(
	&self,
	client: &Client,
	dest: &ServerName,
	request: T,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	let result = self
		.execute_uncounted(client, dest, request)
		.await;

	match &result {
		| Ok(_) => self.record_success(dest).await,
		| Err(error) =>
			if let Some(class) = classify_error(error) {
				self.record_failure(dest, class);
			},
	}

	result
}

/// Like [`execute_on`] but leaves peer-status untouched, for callers that
/// must honor backoff without contributing to it.
#[implement(super::Service)]
#[tracing::instrument(
	name = "fed",
	level = INFO_SPAN_LEVEL,
	skip(self, client, request),
)]
async fn execute_uncounted<T>(
	&self,
	client: &Client,
	dest: &ServerName,
	request: T,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	if !self.services.server.config.allow_federation {
		return Err!(Config("allow_federation", "Federation is disabled."));
	}

	if self
		.services
		.server
		.config
		.is_forbidden_remote_server_name(dest)
	{
		return Err!(Request(Forbidden(debug_warn!("Federation with {dest} is not allowed."))));
	}

	let actual = self
		.services
		.resolver
		.get_actual_dest(dest)
		.await?;

	let request = self.prepare(&actual, dest, request)?;

	self.perform::<T>(&actual, dest, request, client)
		.await
}

#[implement(super::Service)]
async fn perform<T>(
	&self,
	actual: &ActualDest,
	dest: &ServerName,
	request: Request,
	client: &Client,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	let url = request.url().clone();
	let method = request.method().clone();

	debug!(?method, ?url, "Sending request");
	let limit = self.services.server.config.max_response_size;

	match client.execute(request).await {
		| Ok(response) => handle_response::<T>(actual, dest, &method, &url, response, limit)
			.await
			.inspect_err(|error| self.evict_misrouted(dest, actual, error)),
		| Err(error) => Err(self
			.handle_error(dest, actual, &method, &url, error)
			.expect_err("always returns error")),
	}
}

#[implement(super::Service)]
fn prepare<T>(&self, actual: &ActualDest, dest: &ServerName, request: T) -> Result<Request>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	let request = self.to_http_request::<T>(actual, dest, request)?;
	let request = Request::try_from(request)?;
	self.validate_url(request.url())?;
	self.services.server.check_running()?;

	Ok(request)
}

#[implement(super::Service)]
fn validate_url(&self, url: &Url) -> Result {
	if let Some(url_host) = url.host_str()
		&& let Ok(ip) = IPAddress::parse(url_host)
	{
		trace!("Checking request URL IP {ip:?}");
		self.services.resolver.validate_ip(&ip)?;
	}

	Ok(())
}

async fn handle_response<T>(
	actual: &ActualDest,
	dest: &ServerName,
	method: &Method,
	url: &Url,
	response: Response,
	limit: usize,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	let response = into_http_response(dest, actual, method, url, response, limit).await?;

	T::IncomingResponse::try_from_http_response(response)
		.map_err(|e| err!(BadServerResponse("Server returned bad 200 response: {e:?}")))
}

async fn into_http_response(
	dest: &ServerName,
	actual: &ActualDest,
	method: &Method,
	url: &Url,
	mut response: Response,
	limit: usize,
) -> Result<http::Response<Bytes>> {
	let status = response.status();
	trace!(
		?status, ?method,
		request_url = ?url,
		response_url = ?response.url(),
		"Received response from {}",
		actual.to_string(),
	);

	let mut http_response_builder = http::Response::builder()
		.status(status)
		.version(response.version());

	mem::swap(
		response.headers_mut(),
		http_response_builder
			.headers_mut()
			.expect("http::response::Builder is usable"),
	);

	// TODO: handle timeout
	trace!("Waiting for response body...");
	let body = read_response_capped(response, limit).await?;

	let http_response = http_response_builder
		.body(body)
		.expect("reqwest body is valid http body");

	debug!("Got {status:?} for {method} {url}");
	if !status.is_success() {
		return Err(Error::Federation(
			dest.to_owned(),
			RumaError::from_http_response(http_response),
		));
	}

	Ok(http_response)
}

#[implement(super::Service)]
fn handle_error(
	&self,
	dest: &ServerName,
	actual: &ActualDest,
	method: &Method,
	url: &Url,
	mut e: reqwest::Error,
) -> Result {
	if e.is_timeout() || e.is_connect() {
		e = e.without_url();
		debug_warn!("{e:?}");
	} else if e.is_redirect() {
		debug_error!(
			method = ?method,
			url = ?url,
			final_url = ?e.url(),
			"Redirect loop {}: {}",
			actual.host,
			e,
		);
	} else {
		debug_error!("{e:?}");
	}

	self.evict_route(dest, actual);

	Err(e.into())
}

// A non-JSON federation response means a proxy or CDN answered, not the
// homeserver, so the cached route is stale; evict it as transport errors do.
#[implement(super::Service)]
fn evict_misrouted(&self, dest: &ServerName, actual: &ActualDest, error: &Error) {
	let Error::Federation(_, response) = error else {
		return;
	};

	if matches!(response.body, ErrorBody::NotJson { .. }) {
		self.evict_route(dest, actual);
	}
}

// Overrides are keyed by the resolved (delegated/SRV) hostname, so evict under
// the key resolution wrote (`actual.dest.hostname()`), not the origin name.
#[implement(super::Service)]
fn evict_route(&self, dest: &ServerName, actual: &ActualDest) {
	self.services.resolver.cache.del_destination(dest);
	self.services
		.resolver
		.cache
		.del_override(&actual.dest.hostname());
}

#[implement(super::Service)]
fn to_http_request<T>(
	&self,
	actual: &ActualDest,
	dest: &ServerName,
	request: T,
) -> Result<http::Request<Vec<u8>>>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	const VERSIONS: [MatrixVersion; 1] = [MatrixVersion::V1_11];
	let supported = SupportedVersions {
		versions: VERSIONS.into(),
		features: Default::default(),
	};

	let auth = T::Authentication::input(
		self.services.server.name.clone(),
		dest.to_owned(),
		self.services.server_keys.keypair(),
	);
	let path = T::PathBuilder::input(&supported);

	request
		.try_into_http_request::<Vec<u8>>(actual.to_string().as_str(), auth, path)
		.map_err(|e| err!(BadServerResponse("Invalid destination: {e:?}")))
}
