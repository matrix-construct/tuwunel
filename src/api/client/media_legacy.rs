#![expect(deprecated)]

use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use reqwest::Url;
use ruma::api::client::media::{
	create_content, get_content, get_content_as_filename, get_content_thumbnail,
	get_media_config, get_media_preview,
};
use tuwunel_core::{Err, Result, err, utils::math::ruma_from_usize};
use tuwunel_service::media::{CACHE_CONTROL_IMMUTABLE, CORP_CROSS_ORIGIN};

use crate::{
	Ruma, RumaResponse,
	client::create_content_route,
	utils::{get_file, get_thumbnail},
};

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
	InsecureClientIp(client): InsecureClientIp,
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

/// # `POST /_matrix/media/v1/upload`
///
/// Permanently save media in the server.
///
/// This is a legacy endpoint ("/v1/") that some very old homeservers and/or
/// clients may call. Tuwunel adds these for compatibility purposes.
/// See <https://spec.matrix.org/legacy/legacy/#id27>
///
/// - Some metadata will be saved in the database
/// - Media will be saved in the media/ directory
pub(crate) async fn create_content_legacy_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<create_content::v3::Request>,
) -> Result<RumaResponse<create_content::v3::Response>> {
	create_content_route(State(services), InsecureClientIp(client), body)
		.await
		.map(RumaResponse)
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
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_content::v3::Request>,
) -> Result<get_content::v3::Response> {
	let file_meta =
		get_file(&services, &body.server_name, &body.media_id, None, body.timeout_ms, None)
			.await?;

	Ok(get_content::v3::Response {
		file: file_meta.content.expect("entire file contents"),
		content_type: file_meta.content_type.map(Into::into),
		content_disposition: file_meta.content_disposition,
		cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
		cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
	})
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
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_content_as_filename::v3::Request>,
) -> Result<get_content_as_filename::v3::Response> {
	let file_meta = get_file(
		&services,
		&body.server_name,
		&body.media_id,
		None,
		body.timeout_ms,
		Some(&body.filename),
	)
	.await?;

	Ok(get_content_as_filename::v3::Response {
		file: file_meta.content.expect("entire file contents"),
		content_type: file_meta.content_type.map(Into::into),
		content_disposition: file_meta.content_disposition,
		cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
		cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
	})
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
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_content_thumbnail::v3::Request>,
) -> Result<get_content_thumbnail::v3::Response> {
	let file_meta = get_thumbnail(
		&services,
		&body.server_name,
		&body.media_id,
		None,
		body.timeout_ms,
		body.width,
		body.height,
		body.method.as_ref(),
	)
	.await?;

	Ok(get_content_thumbnail::v3::Response {
		file: file_meta.content.expect("entire file contents"),
		content_type: file_meta.content_type.map(Into::into),
		content_disposition: file_meta.content_disposition,
		cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
		cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
	})
}
