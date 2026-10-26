//! Encodes animated thumbnails.
//!
//! A picture that carries a frame sequence can answer a request for an animated
//! thumbnail with the frames themselves rather than one of them. The three
//! containers MSC2705 names each hold a sequence differently, so each has its
//! own decoder here, and all of them produce a GIF, which is the one animated
//! format this build can encode.
//!
//! The caps that bound the work live here rather than at either call site, so
//! a source reaches no decoder before what it would cost is known.

use std::{
	cmp::min,
	io::Cursor,
	panic::{AssertUnwindSafe, catch_unwind},
	result::Result as StdResult,
};

use bytes::Bytes;
use image::{
	AnimationDecoder, DynamicImage, Frame, Frames, ImageDecoder, ImageFormat, Limits,
	codecs::{
		gif::{GifDecoder, GifEncoder, Repeat},
		png::PngDecoder,
		webp::WebPDecoder,
	},
};
use ruma::Mxc;
use tokio::sync::OwnedSemaphorePermit;
use tuwunel_core::{Err, Error, Result, defer, err, implement};

use super::{
	super::Media,
	Dim,
	animate::GIF,
	generate::{BYTES_PER_PIXEL, reader, thumbnail_generate},
};

const ANIMATED_NAME: &str = "thumbnail.gif";

/// Quantization effort the GIF encoder spends per frame.
///
/// The encoder's own default is 1, which buys quality at any cost; a thumbnail
/// is small and any remote server can ask for one, so this trades a little of
/// that for an encode that ends.
const GIF_SPEED: i32 = 10;

pub(super) struct Output {
	pub(super) media: Result<Media>,
	pub(super) admission: OwnedSemaphorePermit,
	pub(super) source: Bytes,
}

/// Encode this picture's frames as an animated GIF thumbnail and store it.
///
/// Quantization runs per frame on a palette of its own, which is far more work
/// than a still costs, so the encode is handed to a blocking worker rather than
/// run on the async runtime. A source whose frames will not decode answers an
/// error and the caller falls through to the still it would have produced.
#[implement(super::super::Service)]
#[tracing::instrument(
	name = "animate",
	level = "debug",
	skip(self, source, admission)
)]
pub(super) async fn store_animated(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	source: Bytes,
	admission: OwnedSemaphorePermit,
) -> Result<Output> {
	let config = &self.services.config;
	let max_frames = config.media.thumbnail.max_frames;
	let budget = config.media.thumbnail.max_pixels;
	let requested = Dim::new(dim.width, dim.height, Some(dim.method.clone()));

	#[cfg(test)]
	super::tests::before_animation_submit().await;
	#[cfg(test)]
	let control = super::tests::animation_control();

	let encode = self
		.services
		.server
		.runtime()
		.spawn_blocking(move || {
			#[cfg(test)]
			if let Some(control) = control.as_ref() {
				control.worker_started();
			}

			let content = catch_unwind(AssertUnwindSafe(|| {
				encode_frames(&source, &requested, max_frames, budget)
			}))
			.map_err(Error::from_panic)
			.unwrap_or_else(Err);

			#[cfg(test)]
			if let Some(control) = control.as_ref() {
				control.worker_finished();
			}

			(content, admission, source)
		});

	#[cfg(test)]
	super::tests::animation_submitted();

	// a started worker runs to completion, but one still queued is dropped, so
	// a cancelled request must not leave it to reach the pool
	let abort = encode.abort_handle();

	defer! {{ abort.abort(); }}

	let (content, admission, source) = encode.await?;
	let media = match content {
		| Err(error) => Err(error),
		| Ok(content) =>
			self.store_encoded(mxc, dim, content, GIF, ANIMATED_NAME)
				.await,
	};

	Ok(Output { media, admission, source })
}

/// Encode a picture's frames as a GIF scaled to these dimensions.
///
/// Both caps are settled before a frame is pulled, so neither the decode nor
/// the encode can run past them: the sequence stops at the frame count or at
/// what the pixel budget affords, and the animation loops short rather than the
/// request failing. Fewer than two frames is no animation and answers an error.
pub(in super::super) fn encode_frames(
	bytes: &[u8],
	dim: &Dim,
	max_frames: usize,
	budget: u64,
) -> Result<Vec<u8>> {
	let (frames, canvas) = source_frames(bytes, budget)?;

	// every frame composites onto the whole canvas whatever region it updates,
	// so what the budget affords is a count rather than a running total
	let afforded = budget
		.checked_div(canvas)
		.and_then(|afforded| usize::try_from(afforded).ok())
		.unwrap_or(max_frames);

	let limit = min(max_frames, afforded);
	let (content, count) = encode_sequence(frames, dim, limit)?;

	if count < 2 {
		return Err!(debug_warn!(%count, "Picture carries no frame sequence."));
	}

	Ok(content)
}

/// Encodes a bounded frame sequence into one GIF buffer.
///
/// A decode failure ends the sequence where it stands. Encoding failures still
/// abort the result because the output buffer would be incomplete.
fn encode_sequence(frames: Frames<'_>, dim: &Dim, limit: usize) -> Result<(Vec<u8>, usize)> {
	let mut content = Vec::new();
	let count = {
		let mut encoder = GifEncoder::new_with_speed(&mut content, GIF_SPEED);

		encoder
			.set_repeat(Repeat::Infinite)
			.map_err(|error| err!(debug_warn!(?error, "Failed to set the GIF loop count.")))?;

		let count = frames
			.take(limit)
			.map_while(StdResult::ok)
			.try_fold(0_usize, |count, frame| -> Result<usize> {
				let delay = frame.delay();
				let image = DynamicImage::ImageRgba8(frame.into_buffer());
				let scaled = thumbnail_generate(&image, dim)?;

				drop(image);
				encoder
					.encode_frame(Frame::from_parts(scaled.into_rgba8(), 0, 0, delay))
					.map_err(|error| err!(debug_warn!(?error, "Failed to encode a frame.")))?;

				Ok(count.saturating_add(1))
			})?;

		// the encoder writes the trailer as it goes out of scope, so the picture
		// is not complete until it has
		drop(encoder);

		count
	};

	Ok((content, count))
}

/// The frame sequence a picture carries, and what one frame of it costs.
///
/// Only the three containers MSC2705 names can hold a sequence, and two of them
/// say in their own header whether they do, so a still answers an error here
/// rather than yielding a lone frame for the caller to reject. Each decoder is
/// built without limits of its own and composites a whole source canvas on its
/// first advance, so the canvas is budgeted and the decoder told what it may
/// allocate before it is ever advanced.
fn source_frames(bytes: &[u8], budget: u64) -> Result<(Frames<'_>, u64)> {
	let format = reader(bytes)?
		.format()
		.ok_or_else(|| err!(debug_warn!("Picture names no format.")))?;

	let source = Cursor::new(bytes);

	match format {
		| ImageFormat::Gif => gif_frames(source, budget),
		| ImageFormat::Png => png_frames(source, budget),
		| ImageFormat::WebP => webp_frames(source, budget),
		| _ => Err!(debug_warn!(?format, "Format carries no frame sequence.")),
	}
}

/// Builds a bounded GIF frame iterator.
///
/// The canvas is checked before limits are installed or the decoder advances.
fn gif_frames(source: Cursor<&[u8]>, budget: u64) -> Result<(Frames<'_>, u64)> {
	let format = ImageFormat::Gif;
	let failed = |error| err!(debug_warn!(?error, ?format, "Failed to read a frame sequence."));
	let mut decoder = GifDecoder::new(source).map_err(failed)?;
	let canvas = canvas_pixels(&decoder, budget)?;

	decoder
		.set_limits(decoder_limits(budget))
		.map_err(failed)?;

	Ok((decoder.into_frames(), canvas))
}

/// Builds a bounded APNG frame iterator.
///
/// A regular PNG is rejected before the decoder advances.
fn png_frames(source: Cursor<&[u8]>, budget: u64) -> Result<(Frames<'_>, u64)> {
	let format = ImageFormat::Png;
	let failed = |error| err!(debug_warn!(?error, ?format, "Failed to read a frame sequence."));
	let mut decoder = PngDecoder::new(source).map_err(failed)?;
	let canvas = canvas_pixels(&decoder, budget)?;

	if !decoder.is_apng().map_err(failed)? {
		return Err!(debug_warn!("PNG carries a single frame."));
	}

	decoder
		.set_limits(decoder_limits(budget))
		.map_err(failed)?;

	Ok((decoder.apng().map_err(failed)?.into_frames(), canvas))
}

/// Builds a bounded WebP frame iterator.
///
/// A still WebP is rejected before the decoder advances.
fn webp_frames(source: Cursor<&[u8]>, budget: u64) -> Result<(Frames<'_>, u64)> {
	let format = ImageFormat::WebP;
	let failed = |error| err!(debug_warn!(?error, ?format, "Failed to read a frame sequence."));
	let mut decoder = WebPDecoder::new(source).map_err(failed)?;
	let canvas = canvas_pixels(&decoder, budget)?;

	if !decoder.has_animation() {
		return Err!(debug_warn!("WebP carries a single frame."));
	}

	decoder
		.set_limits(decoder_limits(budget))
		.map_err(failed)?;

	Ok((decoder.into_frames(), canvas))
}

/// Creates decoder limits from the configured pixel budget.
///
/// Decoders may allocate four bytes for every pixel they composite.
fn decoder_limits(budget: u64) -> Limits {
	let mut limits = Limits::no_limits();

	limits.max_alloc = Some(budget.saturating_mul(BYTES_PER_PIXEL));

	limits
}

/// Pixels one composited frame of this source occupies.
///
/// A canvas the budget cannot afford even once is refused here, which is before
/// the decoder has been advanced onto it and so before it has allocated one.
fn canvas_pixels(decoder: &impl ImageDecoder, budget: u64) -> Result<u64> {
	let (width, height) = decoder.dimensions();
	let pixels = u64::from(width).saturating_mul(u64::from(height));

	if pixels == 0 || pixels > budget {
		return Err!(
			debug_warn!(%width, %height, %budget, "Canvas is outside the pixel budget.")
		);
	}

	Ok(pixels)
}
