use axum::extract::State;
use bytes::BufMut;
use ruma::api::{
	IncomingRequest, OutgoingResponse,
	auth_scheme::AccessToken,
	error::{FromHttpRequestError, IntoHttpError},
	metadata,
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, http, info};
use tuwunel_service::media::MediaObjectLocations;

use crate::RumaAdmin;

metadata! {
	method: POST,
	rate_limited: false,
	authentication: AccessToken,
	path: "/_tuwunel/admin/v1/media/object-locations",
}

#[derive(Debug, Deserialize)]
pub(crate) struct Request {
	mxc_uris: Vec<String>,
	providers: Option<Vec<String>>,
	#[serde(default = "verify_default")]
	verify: bool,
}

fn verify_default() -> bool { true }

#[derive(Debug, Serialize)]
pub(crate) struct Response {
	locations: Vec<MediaObjectLocations>,
}

// Implement the small server-side boundary directly: ruma's request/response
// derives require crate-wide client/server features intended for API libraries.
impl IncomingRequest for Request {
	type EndpointError = ruma::api::error::Error;
	type OutgoingResponse = Response;

	fn try_from_http_request<B, S>(
		request: http::Request<B>,
		_path_args: &[S],
	) -> std::result::Result<Self, FromHttpRequestError>
	where
		B: AsRef<[u8]>,
		S: AsRef<str>,
	{
		Self::check_request_method(request.method())?;
		Ok(serde_json::from_slice(request.body().as_ref())?)
	}
}

impl OutgoingResponse for Response {
	fn try_into_http_response<T: Default + BufMut>(
		self,
	) -> std::result::Result<http::Response<T>, IntoHttpError> {
		let mut body = T::default();
		serde_json::to_writer((&mut body).writer(), &self)?;

		Ok(http::Response::builder()
			.header(http::header::CONTENT_TYPE, "application/json")
			.header(http::header::CACHE_CONTROL, "no-store")
			.body(body)?)
	}
}

/// # `POST /_tuwunel/admin/v1/media/object-locations`
///
/// Authenticates and authorizes before parsing the batch. Resolution, prefix
/// handling and metadata-only verification belong to the media/storage services.
pub(crate) async fn admin_object_locations_route(
	State(services): State<crate::State>,
	body: RumaAdmin<Request>,
) -> Result<Response> {
	let providers = services
		.media
		.object_location_providers(body.providers.as_deref())?;

	info!(
		administrator = %body.sender_user(),
		?providers,
		item_count = body.mxc_uris.len(),
		verify = body.verify,
		"Resolving media object locations"
	);

	let locations = services
		.media
		.object_locations(&body.mxc_uris, &providers, body.verify)
		.await?;

	Ok(Response { locations })
}
