use std::{fmt::Debug, mem::swap};

use bytes::BytesMut;
use http::Response as HttpResponse;
use reqwest::{Error as ReqwestError, Response};
use ruma::api::{
	IncomingResponse, OutgoingRequest, OutgoingRequestExt, auth_scheme::AuthScheme,
	path_builder::PathBuilder,
};
use tuwunel_core::{
	Err, Result, debug_warn, err, error::error_chain, implement, trace, utils::string_from_bytes,
	warn,
};
use url::Url;

use crate::client::read_response_capped;

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn send_request<T>(&self, dest: &str, request: T) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Debug + Send,
	for<'a> T::Authentication: AuthScheme<Input<'a> = ()>,
	for<'a> T::PathBuilder: PathBuilder<Input<'a> = ()>,
{
	let dest = if dest.contains(['?', '#']) {
		let parsed = Url::parse(dest).ok();

		warn!(
			gateway_host = parsed
				.as_ref()
				.and_then(|url| url.host_str())
				.unwrap_or("<invalid>"),
			has_query = dest.contains('?'),
			has_fragment = dest.contains('#'),
			"Push gateway URL carries a query string or fragment, which is not supported; the \
			 notification path is appended after it",
		);

		dest
	} else {
		let push_path = self
			.services
			.config
			.notification_push_path
			.trim_end_matches('/');

		let dest = dest.trim_end_matches('/');

		dest.strip_suffix(push_path).unwrap_or(dest)
	};

	trace!("Push gateway destination: {dest}");

	let http_request = request
		.try_into_http_request::<BytesMut>(dest, (), ())
		.map_err(|e| {
			err!(BadServerResponse(warn!(
				"Failed to find destination {dest} for push gateway: {e}"
			)))
		})?
		.map(BytesMut::freeze);

	let reqwest_request = reqwest::Request::try_from(http_request)?;

	if self
		.services
		.client
		.proxy
		.resolver_alias(reqwest_request.url())
	{
		return Err!(BadServerResponse(
			"Not allowed to request a locally resolved proxy endpoint"
		));
	}

	trace!("Checking request URL for IP");
	if !self
		.services
		.client
		.valid_cidr_range_url(reqwest_request.url())
	{
		return Err!(BadServerResponse("Not allowed to send requests to this IP"));
	}

	match self
		.services
		.client
		.pusher
		.execute(reqwest_request)
		.await
	{
		| Err(error) => handler_err(dest, error),
		| Ok(response) => self.handle_ok::<T>(dest, response).await,
	}
}

#[implement(super::Service)]
async fn handle_ok<T>(&self, dest: &str, mut response: Response) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest,
{
	trace!("Checking response destination's IP");
	if let Some(remote_addr) = response.remote_addr()
		&& !self
			.services
			.client
			.valid_cidr_range_ip(remote_addr.ip())
		&& !self.services.client.proxied(response.url())
	{
		return Err!(BadServerResponse("Not allowed to send requests to this IP"));
	}

	let status = response.status();
	let mut http_response_builder = HttpResponse::builder()
		.status(status)
		.version(response.version());

	swap(
		response.headers_mut(),
		http_response_builder
			.headers_mut()
			.expect("http::response::Builder is usable"),
	);

	let limit = self.services.config.max_response_size;
	let body = read_response_capped(response, limit).await?;

	if !status.is_success() {
		debug_warn!(body = ?string_from_bytes(&body), "Push gateway response");
		return Err!(BadServerResponse(warn!(
			"Push gateway {dest} returned unsuccessful HTTP response: {status}"
		)));
	}

	let response = T::IncomingResponse::try_from_http_response(
		http_response_builder
			.body(body)
			.expect("reqwest body is valid http body"),
	);

	response.map_err(|e| {
		err!(BadServerResponse(warn!("Push gateway {dest} returned invalid response: {e}")))
	})
}

fn handler_err<R>(dest: &str, error: ReqwestError) -> Result<R> {
	warn!(%dest, chain = %error_chain(&error), "Could not send request to pusher");
	Err(error.into())
}
