use std::{fmt::Debug, mem};

use bytes::BytesMut;
use ipaddress::IPAddress;
use ruma::api::{
	IncomingResponse, MatrixVersion, OutgoingRequest, SendAccessToken, SupportedVersions,
};
use tuwunel_core::{
	Err, Result, debug_warn, err, implement, trace, utils::string_from_bytes, warn,
};

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn send_request<T>(&self, dest: &str, request: T) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Debug + Send,
{
	const VERSIONS: [MatrixVersion; 1] = [MatrixVersion::V1_0];
	let supported = SupportedVersions {
		versions: VERSIONS.into(),
		features: Default::default(),
	};

	let dest = dest.replace(&self.services.config.notification_push_path, "");
	trace!("Push gateway destination: {dest}");

	let http_request = request
		.try_into_http_request::<BytesMut>(&dest, SendAccessToken::IfRequired(""), &supported)
		.map_err(|e| {
			err!(BadServerResponse(warn!(
				"Failed to find destination {dest} for push gateway: {e}"
			)))
		})?
		.map(BytesMut::freeze);

	let reqwest_request = reqwest::Request::try_from(http_request)?;
	if let Some(url_host) = reqwest_request.url().host_str() {
		trace!("Checking request URL for IP");
		if let Ok(ip) = IPAddress::parse(url_host)
			&& !self.services.client.valid_cidr_range(&ip)
		{
			return Err!(BadServerResponse("Not allowed to send requests to this IP"));
		}
	}

	let response = self
		.services
		.client
		.pusher
		.execute(reqwest_request)
		.await;

	match response {
		| Ok(mut response) => {
			// reqwest::Response -> http::Response conversion

			trace!("Checking response destination's IP");
			if let Some(remote_addr) = response.remote_addr()
				&& let Ok(ip) = IPAddress::parse(remote_addr.ip().to_string())
				&& !self.services.client.valid_cidr_range(&ip)
			{
				return Err!(BadServerResponse("Not allowed to send requests to this IP"));
			}

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

			let body = response.bytes().await?; // TODO: handle timeout

			if !status.is_success() {
				debug_warn!("Push gateway response body: {:?}", string_from_bytes(&body));
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
				err!(BadServerResponse(warn!(
					"Push gateway {dest} returned invalid response: {e}"
				)))
			})
		},
		| Err(e) => {
			warn!("Could not send request to pusher {dest}: {e}");
			Err(e.into())
		},
	}
}
