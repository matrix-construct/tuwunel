use std::{fmt::Debug, mem};

use bytes::{Bytes, BytesMut};
use ipaddress::IPAddress;
use ruma::api::{
	IncomingResponse, OutgoingRequest,
	auth_scheme::AuthScheme,
	path_builder::PathBuilder,
	push_gateway::send_event_notification::v1::{Notification, Request, Response},
};
use tuwunel_core::{
	Err, Result, debug_warn, err, implement, trace, utils::string_from_bytes, warn,
};

use crate::client::read_response_capped;

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn send_request<T>(&self, dest: &str, request: T) -> Result<T::IncomingResponse>
where
	T: OutgoingRequest + Debug + Send,
	for<'a> T::Authentication: AuthScheme<Input<'a> = ()>,
	for<'a> T::PathBuilder: PathBuilder<Input<'a> = ()>,
{
	let (dest, http_request) = self.build_request(dest, request)?;
	let reqwest_request = reqwest::Request::try_from(http_request)?;
	let response = self
		.execute_request(reqwest_request, &dest)
		.await?;
	T::IncomingResponse::try_from_http_response(response).map_err(|e| {
		err!(BadServerResponse(warn!("Push gateway {dest} returned invalid response: {e}")))
	})
}

#[implement(super::Service)]
pub(super) async fn send_raw_request(&self, dest: &str, body: Vec<u8>) -> Result<Response> {
	// Build from the Ruma endpoint request so custom notification_push_path
	// stripping, the spec path, method, and headers cannot drift from events.
	let template = Request::new(Notification::new(Vec::new()));
	let (dest, mut request) = self.build_request(dest, template)?;
	*request.body_mut() = body.into();

	let request = reqwest::Request::try_from(request)?;
	let response = self.execute_request(request, &dest).await?;
	Response::try_from_http_response(response).map_err(|e| {
		err!(BadServerResponse(warn!("Push gateway {dest} returned invalid response: {e}")))
	})
}

#[implement(super::Service)]
fn build_request<T>(&self, dest: &str, request: T) -> Result<(String, http::Request<Bytes>)>
where
	T: OutgoingRequest + Debug + Send,
	for<'a> T::Authentication: AuthScheme<Input<'a> = ()>,
	for<'a> T::PathBuilder: PathBuilder<Input<'a> = ()>,
{
	let dest = configured_destination(dest, &self.services.config.notification_push_path);
	trace!("Push gateway destination: {dest}");

	let request = request
		.try_into_http_request::<BytesMut>(&dest, (), ())
		.map_err(|e| {
			err!(BadServerResponse(warn!(
				"Failed to find destination {dest} for push gateway: {e}"
			)))
		})?
		.map(BytesMut::freeze);

	Ok((dest, request))
}

fn configured_destination(dest: &str, notification_push_path: &str) -> String {
	dest.replace(notification_push_path, "")
}

#[implement(super::Service)]
async fn execute_request(
	&self,
	reqwest_request: reqwest::Request,
	dest: &str,
) -> Result<http::Response<Bytes>> {
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

			let limit = self.services.config.max_response_size;
			let body = read_response_capped(response, limit).await?;

			if !status.is_success() {
				debug_warn!("Push gateway response body: {:?}", string_from_bytes(&body));
				return Err!(BadServerResponse(warn!(
					"Push gateway {dest} returned unsuccessful HTTP response: {status}"
				)));
			}

			Ok(http_response_builder
				.body(body)
				.expect("reqwest body is valid http body"))
		},
		| Err(e) => {
			warn!("Could not send request to pusher {dest}: {e}");
			Err(e.into())
		},
	}
}

#[cfg(test)]
mod tests {
	use super::configured_destination;

	#[test]
	fn custom_notification_path_is_stripped_before_endpoint_rebuild() {
		assert_eq!(
			configured_destination(
				"https://push.example/custom/push/notify",
				"/custom/push/notify",
			),
			"https://push.example"
		);
	}
}
