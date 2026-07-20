use std::{fmt::Debug, mem};

use bytes::{Bytes, BytesMut};
use reqwest::Request;
use ruma::api::{
	IncomingResponse, OutgoingRequest, OutgoingRequestExt,
	appservice::Registration,
	auth_scheme::{AuthScheme, SendAccessToken},
	path_builder::PathBuilder,
};
use tuwunel_core::{
	Err, Result, debug_error, err, implement, trace, utils::string_from_bytes, warn,
};

use crate::client::read_response_capped;

/// Sends a request to an appservice
///
/// Only returns Ok(None) if there is no url specified in the appservice
/// registration file
#[implement(super::Service)]
pub async fn send_request<T>(
	&self,
	registration: Registration,
	request: T,
) -> Result<Option<T::IncomingResponse>>
where
	T: OutgoingRequest + Debug + Send,
	for<'a> T::Authentication: AuthScheme<Input<'a> = SendAccessToken<'a>>,
	for<'a> T::PathBuilder: PathBuilder<Input<'a> = ()>,
{
	let client = &self.services.client.appservice;

	let Some(dest) = registration.url else {
		return Ok(None);
	};

	if dest == *"null" || dest.is_empty() {
		return Ok(None);
	}

	trace!("Appservice URL \"{dest}\", Appservice ID: {}", registration.id);

	let hs_token = registration.hs_token.as_str();
	let mut http_request = request
		.try_into_http_request::<BytesMut>(&dest, SendAccessToken::IfRequired(hs_token), ())
		.map_err(|e| {
			err!(BadServerResponse(
				warn!(appservice = %registration.id, "Failed to find destination {dest}: {e:?}")
			))
		})?
		.map(BytesMut::freeze);

	add_access_token_query(&mut http_request, hs_token);

	let reqwest_request = Request::try_from(http_request)?;

	let mut response = client
		.execute(reqwest_request)
		.await
		.map_err(|e| {
			warn!(
				"Could not send request to appservice \"{}\" at {dest}: {e:?}",
				registration.id
			);
			e
		})?;

	// reqwest::Response -> http::Response conversion
	let status = response.status();
	let mut http_response_builder = http::Response::builder()
		.status(status)
		.version(response.version());

	mem::swap(
		response.headers_mut(),
		http_response_builder
			.headers_mut()
			.expect("http::response::Builder is usable"),
	);

	let limit = self.services.config.max_response_size;
	let body = read_response_capped(response, limit).await?;

	if !status.is_success() {
		debug_error!("Appservice response bytes: {:?}", string_from_bytes(&body));
		return Err!(BadServerResponse(warn!(
			"Appservice \"{}\" returned unsuccessful HTTP response {status} at {dest}",
			registration.id
		)));
	}

	let response = T::IncomingResponse::try_from_http_response(
		http_response_builder
			.body(body)
			.expect("reqwest body is valid http body"),
	);

	response.map(Some).map_err(|e| {
		err!(BadServerResponse(warn!(
			"Appservice \"{}\" returned invalid/malformed response bytes {dest}: {e}",
			registration.id
		)))
	})
}

/// Appends the `hs_token` as an `access_token` query parameter, the legacy
/// authentication scheme some appservices still require.
pub(super) fn add_access_token_query(request: &mut http::Request<Bytes>, hs_token: &str) {
	let mut parts = request.uri().clone().into_parts();
	let old_path_and_query = parts
		.path_and_query
		.expect("valid request uri path and query")
		.as_str()
		.to_owned();

	let symbol = if old_path_and_query.contains('?') { "&" } else { "?" };

	parts.path_and_query = Some(
		format!("{old_path_and_query}{symbol}access_token={hs_token}")
			.parse()
			.expect("valid path and query"),
	);

	*request.uri_mut() = parts
		.try_into()
		.expect("our manipulation is always valid");
}
