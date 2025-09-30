use std::time::Duration;

use ruma::{Mxc, ServerName, UInt, UserId, media::Method};
use tuwunel_core::{Result, utils::content_disposition::make_content_disposition};
use tuwunel_service::{
	Services,
	media::{Dim, FileMeta},
};

pub(crate) async fn get_file(
	services: &Services,
	server_name: &ServerName,
	media_id: &str,
	user: Option<&UserId>,
	timeout_ms: Duration,
	filename: Option<&str>,
) -> Result<FileMeta> {
	let mxc = Mxc { server_name, media_id };
	let mut file_meta = services
		.media
		.get_or_fetch_file_meta(&mxc, user, timeout_ms)
		.await?;

	transform_content_disposition(&mut file_meta, filename);

	Ok(file_meta)
}

pub(crate) async fn get_thumbnail(
	services: &Services,
	server_name: &ServerName,
	media_id: &str,
	user: Option<&UserId>,
	timeout_ms: Duration,
	width: UInt,
	height: UInt,
	method: Option<&Method>,
) -> Result<FileMeta> {
	let mxc = Mxc { server_name, media_id };

	let dim = Dim::from_ruma(width, height, method.cloned())?;

	let mut file_meta = services
		.media
		.get_or_fetch_thumbnail_meta(&mxc, user, timeout_ms, &dim)
		.await?;

	transform_content_disposition(&mut file_meta, None);

	Ok(file_meta)
}

fn transform_content_disposition(file_meta: &mut FileMeta, filename: Option<&str>) {
	file_meta.content_disposition = Some(make_content_disposition(
		file_meta.content_disposition.as_ref(),
		file_meta.content_type.as_deref(),
		filename,
	));
}
