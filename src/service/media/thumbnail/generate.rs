//! Generates and stores still and animated thumbnail variants.

use std::{cmp::min, io::Cursor};

use image::{DynamicImage, ImageFormat, ImageReader, Limits, imageops::FilterType};
use ruma::{
	Mxc,
	http_headers::{ContentDisposition, ContentDispositionType},
	media::Method,
};
use tuwunel_core::{Err, Result, err, implement, utils::BoolExt};

#[cfg(test)]
use super::tests::caller_after_animation;
use super::{
	super::{Media, data::Metadata},
	Animate, Dim,
	request::into_media,
	sniff::{Sequence, sequence},
};

/// Content type of every still thumbnail tuwunel generates.
const PNG: &str = "image/png";

/// Bytes the decoder is budgeted per pixel of the picture it is asked for.
pub(super) const BYTES_PER_PIXEL: u64 = 4;

/// Filename a generated still thumbnail is disposed under, per the media
/// repository specification, rather than the source filename.
const STILL_NAME: &str = "thumbnail.png";

/// The size a picture that may not be served stands in at.
///
/// Only the header is read, since the decode this precedes may never happen.
/// A header that will not read at all falls back to the largest bucket, which
/// answers smaller than the request but never with the animation.
pub(super) fn picture_dim(bytes: &[u8]) -> Dim { header_dim(bytes).unwrap_or_else(Dim::largest) }

/// The dimensions this picture's own header declares.
///
/// A header that will not read, or that reads as having no extent, answers
/// `None` rather than a size nothing could be encoded at. Callers that must
/// name a size regardless take [`picture_dim`] instead.
fn header_dim(bytes: &[u8]) -> Option<Dim> {
	reader(bytes)
		.ok()
		.and_then(|reader| reader.into_dimensions().ok())
		.filter(|&(width, height)| width > 0 && height > 0)
		.map(|(width, height)| Dim::new(width, height, Some(Method::Scale)))
}
/// Decodes a picture that may not be served in the state it is held in.
///
/// Serving it is what the request forbade, so a decode that fails leaves
/// nothing answerable and the caller has no fallback to offer.
#[implement(super::super::Service)]
pub(super) fn decode_still(&self, bytes: &[u8]) -> Result<DynamicImage> {
	self.decode(bytes)
		.map_err(|_| err!(Request(NotFound("Media thumbnail not found."))))
}

/// Re-encode a fetched picture as a still thumbnail and store it.
///
/// A peer that ignores the parameter answers a cold fetch with animation, and
/// this repairs it on the way out rather than one request later. The cached
/// row takes its own path, which already holds the picture.
#[implement(super::super::Service)]
#[tracing::instrument(name = "still", level = "debug", skip(self, animated))]
pub(in super::super) async fn store_still(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	animated: Media,
) -> Result<Media> {
	// the sentinel names the original rather than a size to re-encode at, so
	// the picture stands in at its own, as it does on the lookup path
	let standin = dim
		.is_original()
		.then(|| picture_dim(&animated.content));

	let image = self.decode_still(&animated.content)?;

	drop(animated);

	self.store_thumbnail(mxc, standin.as_ref().unwrap_or(dim), image)
		.await
}

/// Generate a thumbnail.
///
/// A source that animates yields both variants here rather than the one this
/// request asked for. Generation runs only on a lookup miss, and the row it
/// stores is what stops the next miss, so a variant left ungenerated would wait
/// on a miss that the other variant has already made impossible.
#[implement(super::super::Service)]
#[tracing::instrument(name = "generate", level = "debug", skip(self, data))]
pub(super) async fn get_thumbnail_generate(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	animate: Animate,
	data: Metadata,
) -> Result<Media> {
	let animation_enabled = self.services.config.media.thumbnail.animated;
	let admission = animation_enabled
		.then_async(async || {
			let slots = self.animated_thumbnail_slots.clone();
			let Ok(admission) = slots.acquire_owned().await else {
				return Err!(debug_warn!("The animated thumbnail semaphore is closed."));
			};

			Ok(admission)
		})
		.await
		.transpose()?;

	let bytes = self.fetch_bytes(&data.key).await?;

	// the gates below read this walk too, so it is taken at most once
	let walk = animation_enabled.then(|| sequence(&bytes));

	let (encoded, admission, bytes) = if animates_at(dim, walk, &bytes)? {
		let permit = admission.expect("animation work has source admission");
		let output = self
			.store_animated(mxc, dim, bytes, permit)
			.await?;

		#[cfg(test)]
		caller_after_animation().await;

		(output.media.ok(), Some(output.admission), output.source)
	} else {
		(None, admission, bytes)
	};

	// the variant this request did not ask for is left stored rather than held,
	// so its buffer is not carried across the still encode below
	let animated = animate.allowed().and(encoded);

	let media = into_media(data, bytes.into());
	let frame = self.video_frame(mxc, dim, &media).await;
	let from_video = frame.is_some();

	let Ok(image) = self.decode(frame.as_deref().unwrap_or(&media.content)) else {
		// a frame the thumbnailer refuses is this video's verdict too; without
		// it the program would run again on the next request for any size
		if from_video {
			self.remember_failure(mxc);
		}

		if let Some(animated) = animated {
			return Ok(animated);
		}

		// no still can be derived from a picture that will not decode, and the
		// original answers in its place unless it is the animation the request
		// forbade
		let named = walk.map(Sequence::names_animation);

		return match animate.accepts_fallback_walk(named, &media.content) {
			| true => Ok(media),
			| false => Err!(Request(NotFound("Media thumbnail not found."))),
		};
	};

	drop(frame);

	// a video is never servable in place of its own thumbnail, so its frame is
	// re-encoded however small it is
	let source = Dim::new(image.width(), image.height(), None);
	let animates = walk.map(Sequence::animates);

	if !from_video
		&& dim.is_passthrough(&source)?
		&& animate.accepts_walk(animates, &media.content)
	{
		return Ok(media);
	}

	// nothing below reads the original, which on the video path is the whole
	// staged file, and the encode and the store must not hold it
	drop(media);

	let still = self.store_thumbnail(mxc, dim, image).await?;

	drop(admission);

	Ok(animated.unwrap_or(still))
}

/// Whether this source yields an animated variant at these dimensions.
///
/// Only a picture whose header says it holds a frame sequence reaches the
/// encoder, so a still never pays a decode that would yield one frame and be
/// thrown away, and a video is excluded by the same test since its container is
/// none of the three that hold frames. A size the request cannot improve on is
/// passed through whole below rather than re-encoded. The caller takes the
/// walk and reads it again at that passthrough, and hands `None` where the
/// feature is off.
fn animates_at(dim: &Dim, walk: Option<Sequence>, content: &[u8]) -> Result<bool> {
	if !walk.is_some_and(Sequence::animates) {
		return Ok(false);
	}

	// the passthrough below is tested against the decoded size, so a header
	// that will not read would leave the two disagreeing over one picture
	let Some(source) = header_dim(content) else {
		return Ok(false);
	};

	dim.is_passthrough(&source)
		.map(|through| !through)
}

/// Encode a still PNG thumbnail at these dimensions and store it.
///
/// The generate path and the still-repair path share this, so a given size
/// carries one content type and one disposition whichever produced it.
#[implement(super::super::Service)]
#[tracing::instrument(name = "store", level = "debug", skip(self, image))]
pub(super) async fn store_thumbnail(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	image: DynamicImage,
) -> Result<Media> {
	let thumbnail = thumbnail_generate(&image, dim)?;

	// the source raster is dead once the thumbnail exists, and neither it nor
	// the thumbnail may be held across the store below
	drop(image);

	let content = encode_png(thumbnail)?;

	self.store_encoded(mxc, dim, content, PNG, STILL_NAME)
		.await
}

/// Encodes one still thumbnail into a PNG buffer.
///
/// The buffer and writer remain confined to this synchronous kernel.
fn encode_png(thumbnail: DynamicImage) -> Result<Vec<u8>> {
	let mut content = Vec::new();
	let () = {
		let mut cursor = Cursor::new(&mut content);

		thumbnail
			.write_to(&mut cursor, ImageFormat::Png)
			.map_err(|error| err!(error!(?error, "Error writing PNG thumbnail.")))?;
	};

	drop(thumbnail);

	Ok(content)
}

/// Store an encoded thumbnail under the type and name it carries.
///
/// Both encoders end here, so a stored thumbnail is disposed inline under the
/// name the media repository specification asks of one whether or not the
/// original arrived with a name of its own.
#[implement(super::super::Service)]
#[tracing::instrument(name = "encoded", level = "debug", skip(self, content))]
pub(super) async fn store_encoded(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	content: Vec<u8>,
	content_type: &str,
	filename: &str,
) -> Result<Media> {
	let content_disposition = ContentDisposition {
		disposition_type: ContentDispositionType::Inline,
		filename: Some(filename.to_owned()),
	};

	let key = self.db.create_file_metadata(
		mxc,
		None,
		dim,
		Some(&content_disposition),
		Some(content_type),
	)?;

	self.create_media_file(&key, &content).await?;

	let media = Media {
		content,
		content_type: Some(content_type.to_owned()),
		content_disposition: Some(content_disposition),
	};

	Ok(media)
}

/// Decode a picture whose header declares no more than the configured pixel
/// count.
///
/// The dimensions are checked before any decoder allocates, since
/// `Limits` enforces only a byte budget and leaves a decoder free to ignore it.
#[implement(super::super::Service)]
#[tracing::instrument(name = "decode", level = "trace", skip_all)]
pub(super) fn decode(&self, bytes: &[u8]) -> Result<DynamicImage> {
	let budget = self.services.config.media.thumbnail.max_pixels;
	let (width, height) = reader(bytes)?
		.into_dimensions()
		.map_err(|error| err!(debug_warn!(?error, "Failed to read picture dimensions.")))?;

	let pixels = u64::from(width).saturating_mul(u64::from(height));

	if pixels > budget {
		return Err!(debug_warn!(%width, %height, "Picture is past the {budget} pixel budget."));
	}

	limited_reader(bytes, budget)?
		.decode()
		.map_err(|error| err!(debug_warn!(?error, "Failed to decode picture.")))
}

/// Creates an image reader with the configured allocation limit.
///
/// The reader is returned before decoding so the caller controls when source
/// allocation begins.
fn limited_reader(bytes: &[u8], budget: u64) -> Result<ImageReader<Cursor<&[u8]>>> {
	let mut reader = reader(bytes)?;

	reader.limits(decoder_limits(budget));

	Ok(reader)
}

/// Creates decoder limits from the configured pixel budget.
///
/// Image decoders may allocate four bytes for every source pixel.
fn decoder_limits(budget: u64) -> Limits {
	let mut limits = Limits::no_limits();

	limits.max_alloc = Some(budget.saturating_mul(BYTES_PER_PIXEL));

	limits
}

/// Creates an image reader for encoded picture bytes.
///
/// The reader infers the source format when recognized. It returns an error
/// only when probing the bytes fails.
pub(super) fn reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>> {
	ImageReader::new(Cursor::new(bytes))
		.with_guessed_format()
		.map_err(Into::into)
}

/// Resizes a decoded picture to the requested dimensions.
///
/// Scaling preserves aspect ratio, while cropping fills the requested aspect
/// ratio within the source bounds. Both methods avoid upscaling.
pub(in super::super) fn thumbnail_generate(
	image: &DynamicImage,
	requested: &Dim,
) -> Result<DynamicImage> {
	let thumbnail = if !requested.crop() {
		let Dim { width, height, .. } = requested.scaled(&Dim {
			width: image.width(),
			height: image.height(),
			..Dim::default()
		})?;

		image.thumbnail_exact(width, height)
	} else {
		// upscaling is forbidden outright, and resize_to_fill enlarges a source
		// smaller than the request to meet it
		let width = min(requested.width, image.width());
		let height = min(requested.height, image.height());

		image.resize_to_fill(width, height, FilterType::CatmullRom)
	};

	Ok(thumbnail)
}
