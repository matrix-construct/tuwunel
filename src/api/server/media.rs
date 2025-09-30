use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use ruma::api::federation::authenticated_media::{
	Content, ContentMetadata, FileOrLocation, get_content, get_content_thumbnail,
};
use tuwunel_core::Result;

use crate::{
	Ruma,
	utils::{get_file, get_thumbnail},
};

/// # `GET /_matrix/federation/v1/media/download/{mediaId}`
///
/// Load media from our server.
#[tracing::instrument(
	name = "media_get",
	level = "debug",
	skip_all,
	fields(%client)
)]
pub(crate) async fn get_content_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_content::v1::Request>,
) -> Result<get_content::v1::Response> {
	let file_meta = get_file(
		&services,
		services.globals.server_name(),
		&body.media_id,
		None,
		body.timeout_ms,
		None,
		false,
	)
	.await?;

	Ok(get_content::v1::Response {
		content: FileOrLocation::File(Content {
			file: file_meta.content.expect("entire file contents"),
			content_type: file_meta.content_type.map(Into::into),
			content_disposition: file_meta.content_disposition,
		}),
		metadata: ContentMetadata::new(),
	})
}

/// # `GET /_matrix/federation/v1/media/thumbnail/{mediaId}`
///
/// Load media thumbnail from our server.
#[tracing::instrument(
	name = "media_thumbnail_get",
	level = "debug",
	skip_all,
	fields(%client)
)]
pub(crate) async fn get_content_thumbnail_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_content_thumbnail::v1::Request>,
) -> Result<get_content_thumbnail::v1::Response> {
	let file_meta = get_thumbnail(
		&services,
		services.globals.server_name(),
		&body.media_id,
		None,
		body.timeout_ms,
		body.width,
		body.height,
		body.method.as_ref(),
		false,
	)
	.await?;

	Ok(get_content_thumbnail::v1::Response {
		content: FileOrLocation::File(Content {
			file: file_meta.content.expect("entire file contents"),
			content_type: file_meta.content_type.map(Into::into),
			content_disposition: file_meta.content_disposition,
		}),
		metadata: ContentMetadata::new(),
	})
}
