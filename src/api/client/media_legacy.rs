#![expect(deprecated)]

use axum::{
	extract::State,
	response::{IntoResponse, Redirect, Response},
};
use reqwest::Url;
use ruma::{
	Mxc,
	api::client::media::{
		get_content, get_content_as_filename,
		get_content_thumbnail::{self, v3::Response as ThumbnailResponse},
		get_media_config, get_media_preview,
	},
};
use tuwunel_core::{
	Err, Result, err,
	utils::{content_disposition::make_content_disposition, math::ruma_from_usize},
};
use tuwunel_service::media::{Animate, CACHE_CONTROL_IMMUTABLE, CORP_CROSS_ORIGIN, Dim, Media};

use crate::{ClientIp, Ruma, RumaResponse};

/// # `GET /_matrix/media/v3/config`
///
/// Returns max upload size.
pub(crate) async fn get_media_config_legacy_route(
	State(services): State<crate::State>,
	_body: Ruma<get_media_config::v3::Request>,
) -> Result<get_media_config::v3::Response> {
	Ok(get_media_config::v3::Response {
		upload_size: ruma_from_usize(services.server.config.max_request_size),
	})
}

/// # `GET /_matrix/media/v3/preview_url`
///
/// Returns URL preview.
#[tracing::instrument(skip_all, fields(%client), name = "url_preview_legacy", level = "debug")]
pub(crate) async fn get_media_preview_legacy_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<get_media_preview::v3::Request>,
) -> Result<get_media_preview::v3::Response> {
	let sender_user = body.sender_user();

	let url = &body.url;
	let url = Url::parse(&body.url).map_err(|e| {
		err!(Request(InvalidParam(
			debug_warn!(%sender_user, %url, "Requested URL is not valid: {e}")
		)))
	})?;

	if !services.media.url_preview_allowed(&url) {
		return Err!(Request(Forbidden(
			debug_warn!(%sender_user, %url, "URL is not allowed to be previewed")
		)));
	}

	let preview = services
		.media
		.get_url_preview(&url)
		.await
		.map_err(|e| {
			err!(Request(Unknown(
				debug_error!(%sender_user, %url, "Failed to fetch a URL preview: {e}")
			)))
		})?;

	serde_json::value::to_raw_value(&preview)
		.map(get_media_preview::v3::Response::from_raw_value)
		.map_err(|error| {
			err!(Request(Unknown(
				debug_error!(%sender_user, %url, "Failed to parse URL preview: {error}")
			)))
		})
}

/// # `GET /_matrix/media/v3/download/{serverName}/{mediaId}`
///
/// Load media from our server or over federation.
///
/// - Only allows federation if `allow_remote` is true
/// - Only redirects if `allow_redirect` is true
/// - Uses client-provided `timeout_ms` if available, else defaults to 20
///   seconds
#[tracing::instrument(skip_all, fields(%client), name = "media_get_legacy", level = "debug")]
pub(crate) async fn get_content_legacy_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<get_content::v3::Request>,
) -> Result<Response> {
	let mxc = Mxc {
		server_name: &body.server_name,
		media_id: &body.media_id,
	};

	if body.allow_redirect
		&& services.globals.server_is_ours(&body.server_name)
		&& let Some(url) = services
			.media
			.redirect_url(&mxc, &Dim::default(), Animate::Allowed)
			.await?
	{
		return Ok(Redirect::temporary(url.as_str()).into_response());
	}

	match services
		.media
		.get(&mxc, Some(body.timeout_ms))
		.await
	{
		| Ok(Media {
			content,
			content_type,
			content_disposition,
		}) => {
			let content_disposition = make_content_disposition(
				content_disposition.as_ref(),
				content_type.as_deref(),
				None,
			);

			let response = get_content::v3::Response {
				file: content,
				content_type: content_type.map(Into::into),
				content_disposition: Some(content_disposition),
				cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
				cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
			};

			Ok(RumaResponse(response).into_response())
		},
		| Err(e) =>
			if !services.globals.server_is_ours(&body.server_name) && body.allow_remote {
				let response = services
					.media
					.fetch_remote_content_legacy(&mxc, body.timeout_ms)
					.await
					.map_err(|e| {
						err!(Request(NotFound(debug_warn!(%mxc, "Fetching media failed: {e:?}"))))
					})?;

				let content_disposition = make_content_disposition(
					response.content_disposition.as_ref(),
					response.content_type.as_deref(),
					None,
				);

				let response = get_content::v3::Response {
					file: response.file,
					content_type: response.content_type,
					content_disposition: Some(content_disposition),
					cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
					cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
				};

				Ok(RumaResponse(response).into_response())
			} else {
				Err(e)
			},
	}
}

/// # `GET /_matrix/media/v3/download/{serverName}/{mediaId}/{fileName}`
///
/// Load media from our server or over federation, permitting desired filename.
///
/// - Only allows federation if `allow_remote` is true
/// - Only redirects if `allow_redirect` is true
/// - Uses client-provided `timeout_ms` if available, else defaults to 20
///   seconds
#[tracing::instrument(skip_all, fields(%client), name = "media_get_legacy", level = "debug")]
pub(crate) async fn get_content_as_filename_legacy_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<get_content_as_filename::v3::Request>,
) -> Result<Response> {
	let mxc = Mxc {
		server_name: &body.server_name,
		media_id: &body.media_id,
	};

	if body.allow_redirect
		&& services.globals.server_is_ours(&body.server_name)
		&& let Some(url) = services
			.media
			.redirect_url(&mxc, &Dim::default(), Animate::Allowed)
			.await?
	{
		return Ok(Redirect::temporary(url.as_str()).into_response());
	}

	match services
		.media
		.get(&mxc, Some(body.timeout_ms))
		.await
	{
		| Ok(Media {
			content,
			content_type,
			content_disposition,
		}) => {
			let content_disposition = make_content_disposition(
				content_disposition.as_ref(),
				content_type.as_deref(),
				Some(&body.filename),
			);

			let response = get_content_as_filename::v3::Response {
				file: content,
				content_type: content_type.map(Into::into),
				content_disposition: Some(content_disposition),
				cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
				cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
			};

			Ok(RumaResponse(response).into_response())
		},
		| Err(e) =>
			if !services.globals.server_is_ours(&body.server_name) && body.allow_remote {
				let response = services
					.media
					.fetch_remote_content_legacy(&mxc, body.timeout_ms)
					.await
					.map_err(|e| {
						err!(Request(NotFound(debug_warn!(%mxc, "Fetching media failed: {e:?}"))))
					})?;

				let content_disposition = make_content_disposition(
					response.content_disposition.as_ref(),
					response.content_type.as_deref(),
					None,
				);

				let response = get_content_as_filename::v3::Response {
					content_disposition: Some(content_disposition),
					content_type: response.content_type,
					file: response.file,
					cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
					cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
				};

				Ok(RumaResponse(response).into_response())
			} else {
				Err(e)
			},
	}
}

/// # `GET /_matrix/media/v3/thumbnail/{serverName}/{mediaId}`
///
/// Load media thumbnail from our server or over federation.
///
/// - Only allows federation if `allow_remote` is true
/// - Only redirects if `allow_redirect` is true
/// - Uses client-provided `timeout_ms` if available, else defaults to 20
///   seconds
#[tracing::instrument(skip_all, fields(%client), name = "media_thumbnail_get_legacy", level = "debug")]
pub(crate) async fn get_content_thumbnail_legacy_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<get_content_thumbnail::v3::Request>,
) -> Result<Response> {
	let mxc = Mxc {
		server_name: &body.server_name,
		media_id: &body.media_id,
	};

	let dim = Dim::from_ruma(body.width, body.height, body.method.clone())?;
	let animate = Animate::from(body.animated);
	let ours = services.globals.server_is_ours(&body.server_name);

	if body.allow_redirect
		&& ours
		&& let Some(url) = services
			.media
			.redirect_url(&mxc, &dim, animate)
			.await?
	{
		return Ok(Redirect::temporary(url.as_str()).into_response());
	}

	let Media {
		content,
		content_type,
		content_disposition,
	} = match services
		.media
		.get_thumbnail(&mxc, &dim, animate, Some(body.timeout_ms))
		.await
	{
		| Ok(media) => media,
		| Err(e) if ours || !body.allow_remote => return Err(e),
		| Err(_) => services
			.media
			.fetch_remote_thumbnail_legacy(&mxc, body.timeout_ms, &dim, animate)
			.await
			.map_err(|e| {
				err!(Request(NotFound(debug_warn!(%mxc, ?e, "Fetching media failed"))))
			})?,
	};

	// the served type may not be the one the stored disposition was computed
	// against, since a relabelled row is served under its container's type
	let content_disposition =
		make_content_disposition(content_disposition.as_ref(), content_type.as_deref(), None);

	let response = ThumbnailResponse {
		file: content,
		content_type: content_type.map(Into::into),
		cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
		cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
		content_disposition: Some(content_disposition),
	};

	Ok(RumaResponse(response).into_response())
}
