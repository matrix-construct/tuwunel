//! Resolves, signs, sends, and decodes one outbound federation request.
//!
//! Entry points select an HTTP client plus peer-status posture. Destination
//! validation and resolver-cache eviction remain shared across those postures.

use std::{fmt::Debug, mem, time::Duration};

use bytes::Bytes;
use http::{Request as HttpRequest, StatusCode, header::CONTENT_TYPE};
use reqwest::{Method, Request, Response, Url};
use ruma::{
	ServerName,
	api::{
		EndpointError, IncomingResponseExt, MatrixVersion, OutgoingRequest, OutgoingRequestExt,
		SupportedVersions,
		auth_scheme::AuthScheme,
		error::{Error as RumaError, ErrorBody, ErrorKind},
		federation::authentication::{ServerSignatures, XMatrixSigningInput},
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
use crate::{
	client::{Federation, read_response_capped},
	resolver::{actual::ActualDest, fed::FedDest},
};

type RawRequest = HttpRequest<Vec<u8>>;

/// Sends a federation request with the standard federation client.
///
/// The destination is validated and resolved before the request is signed and
/// sent. Success clears peer failures; a failed resolution or send records a
/// classified failure, and a request this server refuses before contacting the
/// peer records nothing. Interrupted errors and errors observed during shutdown
/// record nothing either. This entry point does not itself consult peer
/// backoff.
#[implement(super::Service)]
#[tracing::instrument(skip_all, name = "request", level = "debug")]
pub async fn execute<T>(&self, dest: &ServerName, request: T) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Debug + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	let client = &self.services.client.federation;
	self.execute_on(client, dest, request, None).await
}

/// Sends a federation request that may name this server as its destination.
///
/// Any other destination is served exactly as by [`super::Service::execute`].
/// A room's policy server may share this server's name, so its requests have
/// to reach us whether or not federation loopback is configured.
#[implement(super::Service)]
#[tracing::instrument(skip_all, name = "request", level = "debug")]
pub async fn execute_allow_self<T>(
	&self,
	dest: &ServerName,
	request: T,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Debug + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	let client = &self.services.client.federation;

	self.execute_on_allow_self(client, dest, request, None)
		.await
}

/// Sends a bounded, backoff-aware client key lookup over federation.
///
/// `/keys/query` and `/keys/claim` requests skip servers already in backoff and
/// are limited by `federation_keys_timeout`. The uncounted send path
/// deliberately records neither success, failure, nor timeout, so a slow key
/// lookup does not suppress unrelated outbound traffic to the server.
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

	let timeout_secs = self
		.services
		.server
		.config
		.federation
		.transport
		.keys_timeout;

	let timeout_dur = Duration::from_secs(timeout_secs);

	let client = &self.services.client.federation;

	match timeout(timeout_dur, self.execute_uncounted(client, dest, request, None)).await {
		| Ok(result) => result,
		| Err(_elapsed) => Err!("{dest} key lookup exceeded {}s", timeout_dur.as_secs()),
	}
}

/// Sends a federation request with the long-timeout Synapse client.
///
/// Resolution, signing, response decoding, and peer-status recording match
/// [`super::Service::execute`]; only the selected HTTP client differs.
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
	self.execute_on(client, dest, request, None).await
}

/// Sends through a supplied client and records the peer outcome.
///
/// `limit` can lower but never raise the configured response body limit.
/// The destination's resolved route picks the direct or SRV half of the client.
/// A successful response clears every stored failure row for the destination.
/// Only a failed resolution or send is recorded, and only when classified as a
/// peer failure; a request this server refuses before contacting the peer
/// records nothing, and no backoff gate is consulted before sending.
/// Interrupted errors and errors observed during shutdown record nothing
/// either.
#[implement(super::Service)]
#[tracing::instrument(
	name = "fed",
	level = INFO_SPAN_LEVEL,
	skip(self, client, request),
)]
pub async fn execute_on<T>(
	&self,
	client: &Federation,
	dest: &ServerName,
	request: T,
	limit: Option<usize>,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	self.execute_counted(client, dest, request, false, limit)
		.await
}

/// Executes one Feds or policy-server request while permitting only this
/// server as a self-destination and preserving ordinary peer-status recording.
///
/// Other federation entry points retain the configured loopback gate. A request
/// this server refuses before contacting the peer records nothing.
#[implement(super::Service)]
#[tracing::instrument(name = "fed", level = "debug", skip(self, client, request))]
pub(super) async fn execute_on_allow_self<T>(
	&self,
	client: &Federation,
	dest: &ServerName,
	request: T,
	limit: Option<usize>,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	self.execute_counted(client, dest, request, true, limit)
		.await
}

/// Sends one request, recording only the failures the peer can explain.
///
/// The destination checks run before resolution so that they, like request
/// preparation, refuse with no record. A failed resolution or send records a
/// failure only when classified as a peer failure, and a successful send
/// records success. Interrupted errors and errors observed during shutdown
/// record nothing.
#[implement(super::Service)]
async fn execute_counted<T>(
	&self,
	client: &Federation,
	dest: &ServerName,
	request: T,
	allow_self: bool,
	limit: Option<usize>,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	self.validate_request_destination(dest)?;
	self.services
		.resolver
		.validate_dest(dest, allow_self)?;

	// The checks above decide self-destination once, so a loopback reload
	// cannot turn the resolver's own check into a recorded refusal.
	let actual = self
		.services
		.resolver
		.get_actual_dest_allow_self(dest)
		.await
		.inspect_err(|error| self.record_error(dest, error))?;

	let request = self.prepare(&actual, dest, request)?;
	let result = self
		.perform::<T>(&actual, dest, request, client, limit)
		.await;

	self.record_outcome(dest, result.as_ref().err())
		.await;

	result
}

#[implement(super::Service)]
async fn record_outcome(&self, dest: &ServerName, error: Option<&Error>) {
	match error {
		| None => self.record_success(dest).await,
		| Some(error) => self.record_error(dest, error),
	}
}

#[implement(super::Service)]
pub(super) fn record_error(&self, dest: &ServerName, error: &Error) {
	// A shutdown interrupts requests on this side; the peer did nothing.
	if error.is_interrupted() || self.services.server.is_stopping() {
		return;
	}

	if let Some(class) = classify_error(error) {
		self.record_failure(dest, class);
	}
}

/// Whether a request failed because this server's configuration refused it.
///
/// Only `require_remote_permitted` raises this on the send path, when
/// federation is disabled or the destination is forbidden; a peer's 403 arrives
/// as `Federation`.
#[must_use]
pub fn is_local_refusal(error: &Error) -> bool {
	matches!(error, Error::Request(ErrorKind::Forbidden, ..))
}

/// Sends through a supplied client without changing peer status.
///
/// Callers that gate separately can honor existing backoff without adding
/// success or failure records.
#[implement(super::Service)]
#[tracing::instrument(
	name = "fed",
	level = INFO_SPAN_LEVEL,
	skip(self, client, request),
)]
pub(super) async fn execute_uncounted<T>(
	&self,
	client: &Federation,
	dest: &ServerName,
	request: T,
	limit: Option<usize>,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	self.validate_request_destination(dest)?;
	let actual = self
		.services
		.resolver
		.get_actual_dest(dest)
		.await?;

	let request = self.prepare(&actual, dest, request)?;

	self.perform::<T>(&actual, dest, request, client, limit)
		.await
}

/// Executes one Feds request while permitting only this server as a
/// self-destination and leaving peer status untouched.
///
/// Other federation entry points retain the configured loopback gate.
#[implement(super::Service)]
#[tracing::instrument(name = "fed", level = "debug", skip(self, client, request))]
pub(super) async fn execute_uncounted_allow_self<T>(
	&self,
	client: &Federation,
	dest: &ServerName,
	request: T,
	limit: Option<usize>,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	self.validate_request_destination(dest)?;
	let actual = self
		.services
		.resolver
		.get_actual_dest_allow_self(dest)
		.await?;

	let request = self.prepare(&actual, dest, request)?;

	self.perform::<T>(&actual, dest, request, client, limit)
		.await
}

/// MSC4512: sends a signed federation request built from a raw method, path
/// and body.
///
/// The destination is validated and resolved as for a typed request, and the
/// request signed as this server. The remote's status and body come back
/// whatever the status, the body read up to the response size limit. No
/// backoff is consulted and no outcome recorded, as for an appservice's
/// request no peer state should rest on.
#[implement(super::Service)]
#[tracing::instrument(name = "fed_raw", level = "debug", skip(self, body))]
pub async fn execute_raw(
	&self,
	dest: &ServerName,
	method: Method,
	path_and_query: &str,
	body: Option<Vec<u8>>,
) -> Result<(StatusCode, Bytes)> {
	self.validate_request_destination(dest)?;
	let actual = self
		.services
		.resolver
		.get_actual_dest(dest)
		.await?;

	let request = self.raw_request(&actual, dest, method, path_and_query, body)?;
	let request = Request::try_from(request)?;

	self.validate_url(request.url())?;
	self.services.server.check_running()?;

	let url = request.url().clone();
	let method = request.method().clone();
	let client = &self.services.client.federation;
	let response = match client.for_srv(actual.srv).execute(request).await {
		| Ok(response) => response,
		| Err(error) =>
			return Err(self
				.handle_error(dest, &actual, &method, &url, error)
				.expect_err("always returns error")),
	};

	let limit = self.response_limit(None);

	let status = response.status();
	let body = read_response_capped(response, limit).await?;

	Ok((status, body))
}

#[implement(super::Service)]
fn raw_request(
	&self,
	actual: &ActualDest,
	dest: &ServerName,
	method: Method,
	path_and_query: &str,
	body: Option<Vec<u8>>,
) -> Result<RawRequest> {
	let builder = HttpRequest::builder()
		.method(method)
		.uri(format!("{}{path_and_query}", actual.to_string()));

	let request = match body {
		| None => builder.body(Vec::new())?,
		| Some(body) => builder
			.header(CONTENT_TYPE, "application/json")
			.body(body)?,
	};

	let input = XMatrixSigningInput::new(
		self.services.server.name.clone(),
		dest.to_owned(),
		self.services.server_keys.keypair(),
	);

	signed(request, input)
}

fn signed(mut request: RawRequest, input: XMatrixSigningInput<'_>) -> Result<RawRequest> {
	ServerSignatures::add_authentication(&mut request, input)
		.map_err(|e| err!(BadServerResponse("Failed to sign the federation request: {e}")))?;

	Ok(request)
}

#[implement(super::Service)]
fn validate_request_destination(&self, dest: &ServerName) -> Result {
	self.services
		.server
		.config
		.require_remote_permitted(dest)
}

#[implement(super::Service)]
async fn perform<T>(
	&self,
	actual: &ActualDest,
	dest: &ServerName,
	request: Request,
	client: &Federation,
	limit: Option<usize>,
) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Send,
	T::Authentication: FedAuth,
	T::PathBuilder: FedPath,
{
	let url = request.url().clone();
	let method = request.method().clone();

	debug!(?method, ?url, "Sending request");
	let limit = self.response_limit(limit);

	match client.for_srv(actual.srv).execute(request).await {
		| Ok(response) => handle_response::<T>(actual, dest, &method, &url, response, limit)
			.await
			.inspect_err(|error| self.evict_misrouted(dest, actual, error)),
		| Err(error) => Err(self
			.handle_error(dest, actual, &method, &url, error)
			.expect_err("always returns error")),
	}
}

#[implement(super::Service)]
pub(super) fn prepare<T>(
	&self,
	actual: &ActualDest,
	dest: &ServerName,
	request: T,
) -> Result<Request>
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
pub(super) fn validate_url(&self, url: &Url) -> Result {
	if !self.services.client.valid_cidr_range_url(url) {
		return Err!(BadServerResponse("Not allowed to send requests to this IP"));
	}

	Ok(())
}

/// Determines whether the IP range denylist refuses a destination before any
/// request reaches it.
///
/// Covers the server name when it is an IP literal and the cached route, the
/// two refusals a send raises without contacting the peer; a route not yet
/// cached passes until resolution caches it. A named route's host is read as
/// the request URL reads it, so a numeric form such as `127.1` is an address.
#[implement(super::Service)]
pub async fn denies_route(&self, dest: &ServerName) -> bool {
	let resolver = &self.services.resolver;

	if resolver.denies_dest_address(dest) {
		return true;
	}

	let Ok(cached) = resolver.cache.get_destination(dest).await else {
		return false;
	};

	let client = &self.services.client;

	if let FedDest::Literal(addr) = &cached.dest {
		return !client.valid_cidr_range_ip(addr.ip());
	}

	Url::parse(&cached.dest.https_string()).is_ok_and(|url| !client.valid_cidr_range_url(&url))
}

/// The per-call cap can only narrow the configured ceiling.
#[implement(super::Service)]
pub(super) fn response_limit(&self, cap: Option<usize>) -> usize {
	let global = self
		.services
		.server
		.config
		.network
		.http
		.max_response_size;
	cap.map_or(global, |n| n.min(global))
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

	let (parts, body) = response.into_parts();

	T::IncomingResponse::try_from_http_response(http::Response::from_parts(parts, body.as_ref()))
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
		let (parts, body) = http_response.into_parts();

		return Err(Error::Federation(
			dest.to_owned(),
			RumaError::from_http_response(http::Response::from_parts(parts, body.as_ref())),
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

// Only an SRV route resolves through an override, keyed by the hostname it was
// written under (`actual.dest.hostname()`), not the origin name.
#[implement(super::Service)]
fn evict_route(&self, dest: &ServerName, actual: &ActualDest) {
	let cache = &self.services.resolver.cache;

	cache.del_destination(dest);
	if actual.srv {
		cache.del_override(&actual.dest.hostname());
	}
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
