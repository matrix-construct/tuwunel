#![expect(deprecated)]

use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use reqwest::Url;
use ruma::{
	Mxc,
	api::client::media::{
		create_content, get_content, get_content_as_filename, get_content_thumbnail,
		get_media_config, get_media_preview,
	},
};
use tuwunel_core::{
	Err, Result, err,
	utils::{content_disposition::make_content_disposition, math::ruma_from_usize},
};
use tuwunel_service::media::{CACHE_CONTROL_IMMUTABLE, CORP_CROSS_ORIGIN, Dim, FileMeta};

use crate::{Ruma, RumaResponse, client::create_content_route};

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

/// # `GET /_matrix/media/v1/config`
///
/// This is a legacy endpoint ("/v1/") that some very old homeservers and/or
/// clients may call. Tuwunel adds these for compatibility purposes.
/// See <https://spec.matrix.org/legacy/legacy/#id27>
///
/// Returns max upload size.
pub(crate) async fn get_media_config_legacy_legacy_route(
	State(services): State<crate::State>,
	body: Ruma<get_media_config::v3::Request>,
) -> Result<RumaResponse<get_media_config::v3::Response>> {
	get_media_config_legacy_route(State(services), body)
		.await
		.map(RumaResponse)
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

/// # `GET /_matrix/media/v1/preview_url`
///
/// This is a legacy endpoint ("/v1/") that some very old homeservers and/or
/// clients may call. Tuwunel adds these for compatibility purposes.
/// See <https://spec.matrix.org/legacy/legacy/#id27>
///
/// Returns URL preview.
pub(crate) async fn get_media_preview_legacy_legacy_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_media_preview::v3::Request>,
) -> Result<RumaResponse<get_media_preview::v3::Response>> {
	get_media_preview_legacy_route(State(services), InsecureClientIp(client), body)
		.await
		.map(RumaResponse)
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
	let mxc = Mxc {
		server_name: &body.server_name,
		media_id: &body.media_id,
	};

	match services.media.get(&mxc).await? {
		| Some(FileMeta {
			content,
			content_type,
			content_disposition,
		}) => {
			let content_disposition = make_content_disposition(
				content_disposition.as_ref(),
				content_type.as_deref(),
				None,
			);

			Ok(get_content::v3::Response {
				file: content.expect("entire file contents"),
				content_type: content_type.map(Into::into),
				content_disposition: Some(content_disposition),
				cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
				cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
			})
		},
		| _ =>
			if !services.globals.server_is_ours(&body.server_name) && body.allow_remote {
				let response = services
					.media
					.fetch_remote_content_legacy(&mxc, body.allow_redirect, body.timeout_ms)
					.await
					.map_err(|e| {
						err!(Request(NotFound(debug_warn!(%mxc, "Fetching media failed: {e:?}"))))
					})?;

				let content_disposition = make_content_disposition(
					response.content_disposition.as_ref(),
					response.content_type.as_deref(),
					None,
				);

				Ok(get_content::v3::Response {
					file: response.file,
					content_type: response.content_type,
					content_disposition: Some(content_disposition),
					cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
					cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
				})
			} else {
				Err!(Request(NotFound("Media not found.")))
			},
	}
}

/// # `GET /_matrix/media/v1/download/{serverName}/{mediaId}`
///
/// Load media from our server or over federation.
///
/// This is a legacy endpoint ("/v1/") that some very old homeservers and/or
/// clients may call. Tuwunel adds these for compatibility purposes.
/// See <https://spec.matrix.org/legacy/legacy/#id27>
///
/// - Only allows federation if `allow_remote` is true
/// - Only redirects if `allow_redirect` is true
/// - Uses client-provided `timeout_ms` if available, else defaults to 20
///   seconds
#[tracing::instrument(skip_all, fields(%client), name = "media_get_legacy", level = "debug")]
pub(crate) async fn get_content_legacy_legacy_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_content::v3::Request>,
) -> Result<RumaResponse<get_content::v3::Response>> {
	get_content_legacy_route(State(services), InsecureClientIp(client), body)
		.await
		.map(RumaResponse)
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
	let mxc = Mxc {
		server_name: &body.server_name,
		media_id: &body.media_id,
	};

	match services.media.get(&mxc).await? {
		| Some(FileMeta {
			content,
			content_type,
			content_disposition,
		}) => {
			let content_disposition = make_content_disposition(
				content_disposition.as_ref(),
				content_type.as_deref(),
				Some(&body.filename),
			);

			Ok(get_content_as_filename::v3::Response {
				file: content.expect("entire file contents"),
				content_type: content_type.map(Into::into),
				content_disposition: Some(content_disposition),
				cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
				cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
			})
		},
		| _ =>
			if !services.globals.server_is_ours(&body.server_name) && body.allow_remote {
				let response = services
					.media
					.fetch_remote_content_legacy(&mxc, body.allow_redirect, body.timeout_ms)
					.await
					.map_err(|e| {
						err!(Request(NotFound(debug_warn!(%mxc, "Fetching media failed: {e:?}"))))
					})?;

				let content_disposition = make_content_disposition(
					response.content_disposition.as_ref(),
					response.content_type.as_deref(),
					None,
				);

				Ok(get_content_as_filename::v3::Response {
					content_disposition: Some(content_disposition),
					content_type: response.content_type,
					file: response.file,
					cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
					cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
				})
			} else {
				Err!(Request(NotFound("Media not found.")))
			},
	}
}

/// # `GET /_matrix/media/v1/download/{serverName}/{mediaId}/{fileName}`
///
/// Load media from our server or over federation, permitting desired filename.
///
/// This is a legacy endpoint ("/v1/") that some very old homeservers and/or
/// clients may call. Tuwunel adds these for compatibility purposes.
/// See <https://spec.matrix.org/legacy/legacy/#id27>
///
/// - Only allows federation if `allow_remote` is true
/// - Only redirects if `allow_redirect` is true
/// - Uses client-provided `timeout_ms` if available, else defaults to 20
///   seconds
pub(crate) async fn get_content_as_filename_legacy_legacy_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_content_as_filename::v3::Request>,
) -> Result<RumaResponse<get_content_as_filename::v3::Response>> {
	get_content_as_filename_legacy_route(State(services), InsecureClientIp(client), body)
		.await
		.map(RumaResponse)
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
	let mxc = Mxc {
		server_name: &body.server_name,
		media_id: &body.media_id,
	};

	let dim = Dim::from_ruma(body.width, body.height, body.method.clone())?;
	match services.media.get_thumbnail(&mxc, &dim).await? {
		| Some(FileMeta {
			content,
			content_type,
			content_disposition,
		}) => {
			let content_disposition = make_content_disposition(
				content_disposition.as_ref(),
				content_type.as_deref(),
				None,
			);

			Ok(get_content_thumbnail::v3::Response {
				file: content.expect("entire file contents"),
				content_type: content_type.map(Into::into),
				cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
				cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
				content_disposition: Some(content_disposition),
			})
		},
		| _ =>
			if !services.globals.server_is_ours(&body.server_name) && body.allow_remote {
				let response = services
					.media
					.fetch_remote_thumbnail_legacy(&body)
					.await
					.map_err(|e| {
						err!(Request(NotFound(debug_warn!(%mxc, "Fetching media failed: {e:?}"))))
					})?;

				let content_disposition = make_content_disposition(
					response.content_disposition.as_ref(),
					response.content_type.as_deref(),
					None,
				);

				Ok(get_content_thumbnail::v3::Response {
					file: response.file,
					content_type: response.content_type,
					cross_origin_resource_policy: Some(CORP_CROSS_ORIGIN.into()),
					cache_control: Some(CACHE_CONTROL_IMMUTABLE.into()),
					content_disposition: Some(content_disposition),
				})
			} else {
				Err!(Request(NotFound("Media not found.")))
			},
	}
}

/// # `GET /_matrix/media/v1/thumbnail/{serverName}/{mediaId}`
///
/// Load media thumbnail from our server or over federation.
///
/// This is a legacy endpoint ("/v1/") that some very old homeservers and/or
/// clients may call. Tuwunel adds these for compatibility purposes.
/// See <https://spec.matrix.org/legacy/legacy/#id27>
///
/// - Only allows federation if `allow_remote` is true
/// - Only redirects if `allow_redirect` is true
/// - Uses client-provided `timeout_ms` if available, else defaults to 20
///   seconds
pub(crate) async fn get_content_thumbnail_legacy_legacy_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_content_thumbnail::v3::Request>,
) -> Result<RumaResponse<get_content_thumbnail::v3::Response>> {
	get_content_thumbnail_legacy_route(State(services), InsecureClientIp(client), body)
		.await
		.map(RumaResponse)
}
