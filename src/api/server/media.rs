use axum::extract::State;
use ruma::{
	Mxc,
	api::federation::authenticated_media::{
		Content, ContentMetadata, FileOrLocation, get_content, get_content_thumbnail,
	},
};
use tuwunel_core::{Result, utils::content_disposition::make_content_disposition};
use tuwunel_service::media::{Dim, Media};

use crate::{Ruma, client_ip::ClientIp};

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
	ClientIp(client): ClientIp,
	body: Ruma<get_content::v1::Request>,
) -> Result<get_content::v1::Response> {
	let mxc = Mxc {
		server_name: services.globals.server_name(),
		media_id: &body.media_id,
	};

	let Media {
		content,
		content_type,
		content_disposition,
	} = services.media.get(&mxc, None).await?;

	let content_disposition =
		make_content_disposition(content_disposition.as_ref(), content_type.as_deref(), None);

	let content = Content {
		file: content,
		content_type: content_type.map(Into::into),
		content_disposition: Some(content_disposition),
	};

	Ok(get_content::v1::Response {
		content: FileOrLocation::File(content),
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
	ClientIp(client): ClientIp,
	body: Ruma<get_content_thumbnail::v1::Request>,
) -> Result<get_content_thumbnail::v1::Response> {
	let dim = Dim::from_ruma(body.width, body.height, body.method.clone())?;
	let mxc = Mxc {
		server_name: services.globals.server_name(),
		media_id: &body.media_id,
	};

	let Media {
		content,
		content_type,
		content_disposition,
	} = services
		.media
		.get_thumbnail(&mxc, &dim, None)
		.await?;

	let content_disposition =
		make_content_disposition(content_disposition.as_ref(), content_type.as_deref(), None);

	let content = Content {
		file: content,
		content_type: content_type.map(Into::into),
		content_disposition: Some(content_disposition),
	};

	Ok(get_content_thumbnail::v1::Response {
		content: FileOrLocation::File(content),
		metadata: ContentMetadata::new(),
	})
}
