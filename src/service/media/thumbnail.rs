//! Media Thumbnails
//!
//! This functionality is gated by 'media_thumbnail', but not at the unit level
//! for historical and simplicity reasons. Instead the feature gates the
//! inclusion of dependencies and nulls out results using the existing interface
//! when not featured.

use std::{cmp, num::Saturating as Sat, sync::Arc, time::Duration};

use futures::{StreamExt, pin_mut};
use ruma::{Mxc, UInt, UserId, http_headers::ContentDisposition, media::Method};
use tokio::sync::Notify;
use tuwunel_core::{
	Err, Result, checked, err, implement,
	utils::{result::LogDebugErr, stream::IterStream},
};

use super::{Media, data::Metadata};

/// Dimension specification for a thumbnail.
#[derive(Debug)]
pub struct Dim {
	pub width: u32,
	pub height: u32,
	pub method: Method,
}

impl super::Service {
	/// Uploads or replaces a file thumbnail.
	#[tracing::instrument(
		level = "debug",
		ret(level = "debug")
		skip(self),
	)]
	pub async fn upload_thumbnail(
		&self,
		mxc: &Mxc<'_>,
		user: Option<&UserId>,
		content_disposition: Option<&ContentDisposition>,
		content_type: Option<&str>,
		dim: &Dim,
		file: &[u8],
	) -> Result {
		let key =
			self.db
				.create_file_metadata(mxc, user, dim, content_disposition, content_type)?;

		//TODO: Dangling metadata in database if creation fails
		self.create_media_file(&key, file).await?;
		Ok(())
	}

	#[tracing::instrument(
		level = "debug",
		err(level = "debug")
		skip(self),
	)]
	pub async fn get_or_fetch_thumbnail(
		&self,
		mxc: &Mxc<'_>,
		dim: &Dim,
		timeout_ms: Duration,
		user: &UserId,
	) -> Result<Media> {
		if let Ok(media) = self
			.get_thumbnail(mxc, dim, Some(timeout_ms))
			.await
		{
			return Ok(media);
		}

		if self
			.services
			.globals
			.server_is_ours(mxc.server_name)
		{
			return Err!(Request(NotFound("Local thumbnail not found.")));
		}

		let lock = self.federation_mutex.lock(&mxc.to_string()).await;

		if self
			.db
			.file_metadata_exists(mxc, &dim.normalized())
			.await
		{
			drop(lock);
			return self.get_thumbnail(mxc, dim, None).await;
		}

		self.fetch_remote_thumbnail(mxc, Some(user), None, timeout_ms, dim)
			.await
	}

	/// Download a thumbnail and wait up to a timeout_ms if it is pending.
	#[tracing::instrument(
		level = "debug",
		err(level = "debug")
		skip(self),
	)]
	pub async fn get_thumbnail(
		&self,
		mxc: &Mxc<'_>,
		dim: &Dim,
		timeout_duration: Option<Duration>,
	) -> Result<Media> {
		if let Ok(meta) = self.get_stored_thumbnail(mxc, dim).await {
			return Ok(meta);
		}

		let Some(timeout_duration) = timeout_duration else {
			return Err!(Request(NotFound("Media thumbnail not found.")));
		};

		let Ok(_pending) = self.db.search_pending_mxc(mxc).await else {
			return Err!(Request(NotFound("Media thumbnail not found.")));
		};

		let notifier = self
			.mxc_state
			.notifiers
			.lock()?
			.entry(mxc.to_string().into())
			.or_insert_with(|| Arc::new(Notify::new()))
			.clone();

		if tokio::time::timeout(timeout_duration, notifier.notified())
			.await
			.is_err()
		{
			return Err!(Request(NotYetUploaded("Media has not been uploaded yet")));
		}

		self.get_stored_thumbnail(mxc, dim).await
	}

	/// Downloads a file's thumbnail.
	///
	/// Here's an example on how it works:
	///
	/// - Client requests an image with width=567, height=567
	/// - Server rounds that up to (800, 600), so it doesn't have to save too
	///   many thumbnails
	/// - Server rounds that up again to (958, 600) to fix the aspect ratio
	///   (only for width,height>96)
	/// - Server creates the thumbnail and sends it to the user
	///
	/// For width,height <= 96 the server uses another thumbnailing algorithm
	/// which crops the image afterwards.
	#[tracing::instrument(
		name = "thumbnail",
		level = "debug",
		err(level = "trace")
		skip(self),
	)]
	pub async fn get_stored_thumbnail(&self, mxc: &Mxc<'_>, dim: &Dim) -> Result<Media> {
		// 0, 0 because that's the original file
		let dim = dim.normalized();

		if let Ok(metadata) = self.db.search_file_metadata(mxc, &dim).await {
			return self.get_thumbnail_saved(metadata).await;
		}

		let metadata = self
			.db
			.search_file_metadata(mxc, &Dim::default())
			.await?;

		self.get_thumbnail_generate(mxc, &dim, metadata)
			.await
	}
}

/// Using saved thumbnail
#[implement(super::Service)]
#[tracing::instrument(name = "saved", level = "debug", skip_all)]
async fn get_thumbnail_saved(&self, data: Metadata) -> Result<Media> {
	let path = self.get_media_name_sha256(&data.key);
	let fetch = self
		.storage_providers()
		.stream()
		.filter_map(async |provider| {
			provider
				.get(path.as_str())
				.await
				.log_debug_err()
				.ok()
		});

	pin_mut!(fetch);
	let Some(bytes) = fetch.next().await else {
		return Err!(Request(NotFound("Media thumbnail not found.")));
	};

	Ok(into_media(data, bytes.to_vec()))
}

/// Generate a thumbnail
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
#[tracing::instrument(name = "generate", level = "debug", skip(self, data))]
async fn get_thumbnail_generate(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	data: Metadata,
) -> Result<Media> {
	let Ok(media) = self.get_stored(mxc).await else {
		return Err!("Could not find original media.");
	};

	let Ok(image) = image::load_from_memory(&media.content) else {
		// Couldn't parse file to generate thumbnail, send original
		return Ok(into_media(data, media.content));
	};

	if dim.width > image.width() || dim.height > image.height() {
		return Ok(into_media(data, media.content));
	}

	let mut thumbnail_bytes = Vec::new();
	let thumbnail = thumbnail_generate(&image, dim)?;
	let mut cursor = std::io::Cursor::new(&mut thumbnail_bytes);
	thumbnail
		.write_to(&mut cursor, image::ImageFormat::Png)
		.map_err(|error| err!(error!(?error, "Error writing PNG thumbnail.")))?;

	// Save thumbnail in database so we don't have to generate it again next time
	let thumbnail_key = self.db.create_file_metadata(
		mxc,
		None,
		dim,
		data.content_disposition.as_ref(),
		data.content_type.as_deref(),
	)?;

	self.create_media_file(&thumbnail_key, &thumbnail_bytes)
		.await?;

	Ok(into_media(data, thumbnail_bytes))
}

#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::Service)]
#[tracing::instrument(name = "fallback", level = "debug", skip_all)]
async fn get_thumbnail_generate(
	&self,
	_mxc: &Mxc<'_>,
	_dim: &Dim,
	data: Metadata,
) -> Result<Media> {
	self.get_thumbnail_saved(data).await
}

#[cfg(feature = "media_thumbnail")]
fn thumbnail_generate(
	image: &image::DynamicImage,
	requested: &Dim,
) -> Result<image::DynamicImage> {
	use image::imageops::FilterType;

	let thumbnail = if !requested.crop() {
		let Dim { width, height, .. } = requested.scaled(&Dim {
			width: image.width(),
			height: image.height(),
			..Dim::default()
		})?;
		image.thumbnail_exact(width, height)
	} else {
		image.resize_to_fill(requested.width, requested.height, FilterType::CatmullRom)
	};

	Ok(thumbnail)
}

fn into_media(data: Metadata, content: Vec<u8>) -> Media {
	Media {
		content,
		content_type: data.content_type,
		content_disposition: data.content_disposition,
	}
}

impl Dim {
	/// Instantiate a Dim from Ruma integers with optional method.
	pub fn from_ruma(width: UInt, height: UInt, method: Option<Method>) -> Result<Self> {
		let width = width
			.try_into()
			.map_err(|e| err!(Request(InvalidParam("Width is invalid: {e:?}"))))?;
		let height = height
			.try_into()
			.map_err(|e| err!(Request(InvalidParam("Height is invalid: {e:?}"))))?;

		Ok(Self::new(width, height, method))
	}

	/// Instantiate a Dim with optional method
	#[inline]
	#[must_use]
	pub fn new(width: u32, height: u32, method: Option<Method>) -> Self {
		Self {
			width,
			height,
			method: method.unwrap_or(Method::Scale),
		}
	}

	pub fn scaled(&self, image: &Self) -> Result<Self> {
		let image_width = image.width;
		let image_height = image.height;

		let width = cmp::min(self.width, image_width);
		let height = cmp::min(self.height, image_height);

		let use_width = Sat(width) * Sat(image_height) < Sat(height) * Sat(image_width);

		let x = if use_width {
			let dividend = (Sat(height) * Sat(image_width)).0;
			checked!(dividend / image_height)?
		} else {
			width
		};

		let y = if !use_width {
			let dividend = (Sat(width) * Sat(image_height)).0;
			checked!(dividend / image_width)?
		} else {
			height
		};

		Ok(Self {
			width: x,
			height: y,
			method: Method::Scale,
		})
	}

	/// Returns width, height of the thumbnail and whether it should be cropped.
	/// Returns None when the server should send the original file.
	/// Ignores the input Method.
	#[must_use]
	pub fn normalized(&self) -> Self {
		match (self.width, self.height) {
			| (0..=32, 0..=32) => Self::new(32, 32, Some(Method::Crop)),
			| (0..=96, 0..=96) => Self::new(96, 96, Some(Method::Crop)),
			| (0..=320, 0..=240) => Self::new(320, 240, Some(Method::Scale)),
			| (0..=640, 0..=480) => Self::new(640, 480, Some(Method::Scale)),
			| (0..=800, 0..=600) => Self::new(800, 600, Some(Method::Scale)),
			| _ => Self::default(),
		}
	}

	/// Returns true if the method is Crop.
	#[inline]
	#[must_use]
	pub fn crop(&self) -> bool { self.method == Method::Crop }
}

impl Default for Dim {
	#[inline]
	fn default() -> Self {
		Self {
			width: 0,
			height: 0,
			method: Method::Scale,
		}
	}
}
