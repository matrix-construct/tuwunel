#![cfg(test)]

use ruma::media::Method;

use super::Dim;

fn scale(width: u32, height: u32) -> Dim { Dim::new(width, height, Some(Method::Scale)) }
fn crop(width: u32, height: u32) -> Dim { Dim::new(width, height, Some(Method::Crop)) }
fn source(width: u32, height: u32) -> Dim { Dim::new(width, height, None) }

/// The source already carries the requested dimensions, so scaling is a no-op
/// and re-encoding would only discard the original's format and metadata.
#[test]
fn passthrough_when_scale_request_matches_source() {
	assert!(
		scale(800, 600)
			.is_passthrough(&source(800, 600))
			.unwrap()
	);
}

/// `scaled` covers the requested box rather than fitting inside it, so a source
/// already meeting the requested height cannot shrink: the generated thumbnail
/// would be the source itself.
#[test]
fn passthrough_when_one_side_already_meets_request() {
	assert!(
		scale(800, 600)
			.is_passthrough(&source(1000, 600))
			.unwrap()
	);
}

/// A genuinely larger source must still be thumbnailed.
#[test]
fn generates_when_source_is_larger_in_both_dimensions() {
	assert!(
		!scale(800, 600)
			.is_passthrough(&source(4000, 3000))
			.unwrap()
	);
}

/// Crop reaches the requested size by cropping, so a source that is larger in
/// only one dimension must still be generated. Widening the upscale guard to
/// `>=` would wrongly pass this through.
#[test]
fn generates_when_crop_source_is_larger_in_one_dimension() {
	assert!(
		!crop(96, 96)
			.is_passthrough(&source(500, 96))
			.unwrap()
	);
}

/// Servers must not upscale; a source smaller than the request is served as-is.
#[test]
fn passthrough_when_request_exceeds_source() {
	assert!(
		crop(96, 96)
			.is_passthrough(&source(50, 50))
			.unwrap()
	);
	assert!(
		scale(800, 600)
			.is_passthrough(&source(1000, 400))
			.unwrap()
	);
}

/// Crop at exactly the source size reproduces the source.
#[test]
fn passthrough_when_crop_request_matches_source() {
	assert!(
		crop(96, 96)
			.is_passthrough(&source(96, 96))
			.unwrap()
	);
}

mod animate {
	use super::{
		super::{Animate, thumbnail::ANIMATED_TYPES},
		scale,
	};

	/// MSC2705 gives `animated` three states but only two behaviors.
	///
	/// An explicit `animated=true` is the only one that may be answered with
	/// animation; `false` and an absent parameter alike forbid it.
	#[test]
	fn three_parameter_states_map_to_two_behaviors() {
		assert_eq!(Animate::from(Some(true)), Animate::Allowed);
		assert_eq!(Animate::from(Some(false)), Animate::Never);
		assert_eq!(Animate::from(None), Animate::Never);
	}

	/// A request for a still picture must not be answered with any type an
	/// animation can arrive in.
	///
	/// This is MSC2705's only MUST NOT. The list is the constant itself, so a
	/// type added there is covered on sight.
	#[test]
	fn still_request_withholds_every_animated_type() {
		for content_type in ANIMATED_TYPES {
			assert!(
				!Animate::Never.accepts_type(Some(content_type)),
				"{content_type} must not answer a still request"
			);
		}
	}

	/// Withholding is confined to that family.
	///
	/// The still types a thumbnail is generated as must keep answering every
	/// request, or the rule would withhold the very variant it asks for.
	#[test]
	fn still_request_accepts_still_types() {
		for content_type in
			[Some("image/png"), Some("image/jpeg"), Some("image/png; charset=binary"), None]
		{
			assert!(
				Animate::Never.accepts_type(content_type),
				"{content_type:?} must be servable"
			);
		}
	}

	/// A request that asked for animation is not owed one.
	///
	/// The MSC phrases animation as a SHOULD, so such a request accepts
	/// whichever variant is on hand rather than refusing a still.
	#[test]
	fn animated_request_accepts_anything() {
		for content_type in [Some("image/gif"), Some("image/png"), None] {
			assert!(
				Animate::Allowed.accepts_type(content_type),
				"{content_type:?} must be servable"
			);
		}
	}

	/// Each request prefers the variant it asked for.
	///
	/// A source that animates leaves both stored at a size, and which one a
	/// lookup answers with must not rest on the order their keys fall in, since
	/// a remote row carries the peer's own disposition ahead of its type.
	#[test]
	fn each_request_prefers_its_own_variant() {
		assert!(Animate::Allowed.prefers_type(Some("image/gif")));
		assert!(!Animate::Allowed.prefers_type(Some("image/png")));
		assert!(!Animate::Allowed.prefers_type(None));
		assert!(Animate::Never.prefers_type(Some("image/png")));
		assert!(!Animate::Never.prefers_type(Some("image/gif")));
	}

	/// Withholding survives the shapes a content type arrives in.
	///
	/// These reach the classifier from remote servers and stored keys alike,
	/// so the match tolerates the parameters and casing a peer may attach.
	#[test]
	fn withholding_survives_parameters_and_casing() {
		assert!(!Animate::Never.accepts_type(Some("image/GIF")));
		assert!(!Animate::Never.accepts_type(Some("image/gif; charset=binary")));
		assert!(!Animate::Never.accepts_type(Some("Image/WebP ")));
	}

	/// A type merely spelled like an animated one is a different type.
	///
	/// The match is on the whole essence rather than a prefix, so neither a
	/// longer subtype nor a different top-level type is withheld.
	#[test]
	fn withholding_does_not_reach_beyond_the_family() {
		assert!(Animate::Never.accepts_type(Some("image/gifted")));
		assert!(Animate::Never.accepts_type(Some("video/webp")));
	}

	/// A request too large to thumbnail normalizes to the sentinel.
	///
	/// That sentinel stands for the original file, and is also the key the
	/// original itself is stored under.
	#[test]
	fn oversized_request_normalizes_to_the_original_sentinel() {
		assert!(scale(1200, 900).normalized().is_original());
		assert!(!scale(800, 600).normalized().is_original());
		assert!(!scale(32, 32).normalized().is_original());
	}
}

mod container {
	#[cfg(feature = "media_thumbnail")]
	use super::super::thumbnail::sequence;
	use super::super::{
		Animate,
		thumbnail::{animated_type, animates},
	};

	/// Hand-built headers, since only the header is ever read.
	///
	/// Every container carries a still and an animated form, so a rule taught
	/// to one format cannot quietly pass for another, and WebP carries two
	/// more: an extended header announcing no animation, and an opening chunk
	/// that names no form at all. The bodies and checksums are whatever the
	/// walk skips over rather than real pictures.
	const STILL_PNG: &[u8] =
		b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR0123456789abcCRC1\x00\x00\x00\x00IDATCRC2";
	const ANIMATED_PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR0123456789abcCRC1\x00\x00\x00\x08acTL01234567CRC2\x00\x00\x00\x00IDATCRC3";
	const STILL_WEBP: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 \x00\x00\x00\x00\x00\x00\x00\x00\x00";
	const ANIMATED_WEBP: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00\x02\x00\x00\x00";
	const UNKNOWN_CHUNK_WEBP: &[u8] =
		b"RIFF\x00\x00\x00\x00WEBPICCP\x0a\x00\x00\x00\x00\x00\x00\x00";
	const EXTENDED_STILL_WEBP: &[u8] =
		b"RIFF\x00\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00\xfd\x00\x00\x00";
	const STILL_GIF: &[u8] =
		b"GIF89a\x01\x00\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x00\x3b";
	const ANIMATED_GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x00\x3b";

	/// Bytes a prefix needs before it names which container it is a prefix of.
	///
	/// Below this there is nothing to enter, which is the unrecognized case
	/// rather than the truncated one.
	const NAMED_BYTES: usize = 16;

	/// Sub-blocks to give a still GIF's one frame.
	///
	/// Real pictures run to thousands, each holding at most 255 bytes, so a
	/// walk bounded by a count of them rather than by the picture would call
	/// an ordinary still animated.
	const MANY_SUB_BLOCKS: usize = 512;

	/// One sub-block, holding a single byte of picture data.
	///
	/// A sub-block announces its own length, so this is the smallest one and
	/// the most hops a given amount of data can be made to cost.
	const SUB_BLOCK: &[u8] = b"\x01\x44";

	/// What closes a picture: an empty sub-block, then the trailer.
	///
	/// The empty sub-block ends the data chain and the trailer ends the body,
	/// so a walk reaching this has seen every block the picture holds.
	const PICTURE_END: &[u8] = b"\x00\x3b";

	/// The frame count of an animation is written nowhere in any of the three.
	///
	/// Each format announces it differently, so the three are pinned together
	/// to keep one from being taught a rule that only suits another.
	#[test]
	fn animation_is_read_from_the_container() {
		assert!(animates(ANIMATED_PNG), "an acTL chunk makes a PNG an APNG");
		assert!(animates(ANIMATED_WEBP), "the extended header sets the animation bit");
		assert!(animates(ANIMATED_GIF), "a second image descriptor animates a GIF");
	}

	/// A still of each format is servable, whatever its family can also carry.
	///
	/// This is the half the declared content type gets wrong: `image/webp` and
	/// `image/gif` name families rather than frame counts.
	#[test]
	fn a_still_is_not_withheld_for_the_company_it_keeps() {
		assert!(!animates(STILL_PNG));
		assert!(!animates(STILL_WEBP));
		assert!(!animates(EXTENDED_STILL_WEBP), "an extended header alone is not an animation");
		assert!(!animates(STILL_GIF));
	}

	/// A frame split across many sub-blocks is still one frame.
	///
	/// The walk hops sub-block to sub-block to reach the block after the
	/// picture data, so its work is set by the picture rather than by a count
	/// it could exhaust on an ordinary file.
	#[test]
	fn a_long_frame_does_not_become_an_animation() {
		assert!(!animates(&long_still_gif()));
	}

	/// A still GIF whose one frame is split across many sub-blocks.
	///
	/// The shared still's own ending is cut off and rebuilt, so the picture
	/// differs from it in nothing but how far its data chain runs.
	fn long_still_gif() -> Vec<u8> {
		let head = STILL_GIF
			.get(..STILL_GIF.len().saturating_sub(PICTURE_END.len()))
			.unwrap_or_default();

		let chain = SUB_BLOCK.repeat(MANY_SUB_BLOCKS);

		[head, &chain, PICTURE_END].concat()
	}

	/// A walk stated to a gate answers exactly as reading the bytes does.
	///
	/// The generate path states a walk it already holds and hands `None` where
	/// the feature took none, so the two spellings have to agree on every
	/// container here. The unstated arms are the ones no suite reaches, since
	/// nothing flips the knob that produces them.
	#[cfg(feature = "media_thumbnail")]
	#[test]
	fn a_stated_walk_answers_as_reading_the_bytes_does() {
		let cut = ANIMATED_GIF
			.get(..NAMED_BYTES)
			.unwrap_or(ANIMATED_GIF);

		let containers = [
			STILL_PNG,
			ANIMATED_PNG,
			STILL_WEBP,
			ANIMATED_WEBP,
			EXTENDED_STILL_WEBP,
			STILL_GIF,
			ANIMATED_GIF,
			cut,
		];

		// the two gates part company only where a walk settled nothing, so a
		// set without one would agree for the wrong reason
		let unsettled = containers
			.iter()
			.any(|bytes| animates(bytes) && animated_type(bytes).is_none());

		assert!(unsettled, "no container here left the walk unsettled");

		for bytes in containers {
			for animate in [Animate::Never, Animate::Allowed] {
				gates_agree(animate, bytes);
			}
		}
	}

	/// Both gates answer the same whether handed a walk or the bytes.
	///
	/// Four arms meet here: each gate reads the walk its caller states, or
	/// takes its own where none was stated, and both forms have to agree with
	/// the byte-reading spelling they replaced.
	#[cfg(feature = "media_thumbnail")]
	fn gates_agree(animate: Animate, bytes: &[u8]) {
		let walk = sequence(bytes);
		let len = bytes.len();
		let picture = animate.accepts_picture(bytes);
		let fallback = animate.accepts_fallback(bytes);

		assert_eq!(
			animate.accepts_walk(None, bytes),
			picture,
			"an unstated walk disagreed ({animate:?}, {len} bytes)"
		);

		assert_eq!(
			animate.accepts_walk(Some(walk.animates()), bytes),
			picture,
			"a stated walk disagreed ({animate:?}, {len} bytes)"
		);

		assert_eq!(
			animate.accepts_fallback_walk(None, bytes),
			fallback,
			"an unstated walk disagreed at the fallback ({animate:?}, {len} bytes)"
		);

		assert_eq!(
			animate.accepts_fallback_walk(Some(walk.names_animation()), bytes),
			fallback,
			"a stated walk disagreed at the fallback ({animate:?}, {len} bytes)"
		);
	}

	/// A container whose opening chunk names no known form is withheld.
	///
	/// Only the two plain still chunks and the extended header are recognized,
	/// so anything else has not shown the picture to hold a single frame and
	/// takes the same answer as a truncated one.
	#[test]
	fn an_unknown_chunk_is_not_a_still() {
		assert!(animates(UNKNOWN_CHUNK_WEBP));
		assert!(!animates(EXTENDED_STILL_WEBP), "the extended header is still recognized");
	}

	/// A walk that cannot settle names no type, though it still withholds.
	///
	/// The two questions want opposite defaults: what may be served fails
	/// closed, but what a picture is would be recorded as fact, and a truncated
	/// still would then be stored as the animation it is not.
	#[test]
	fn an_unsettled_walk_names_no_type() {
		let cut = ANIMATED_PNG
			.get(..NAMED_BYTES)
			.unwrap_or(ANIMATED_PNG);

		assert!(animates(cut), "an unsettled walk still withholds");
		assert!(animated_type(cut).is_none(), "but it names nothing");
	}

	/// A picture that will not decode is withheld when it settled as animating.
	///
	/// Nothing can be derived from such a picture, so the original is the only
	/// answer left, and the one thing that answer may not be is the animation
	/// the request forbade. An animation past the decoder's pixel budget needs
	/// no corruption to reach this, so the walk decides rather than the
	/// decoder.
	#[test]
	fn an_undecodable_animation_is_still_withheld() {
		let cut = ANIMATED_GIF
			.get(..NAMED_BYTES)
			.unwrap_or(ANIMATED_GIF);

		assert!(!Animate::Never.accepts_fallback(ANIMATED_GIF), "a settled animation");
		assert!(Animate::Never.accepts_fallback(STILL_GIF), "a settled still answers");
		assert!(Animate::Never.accepts_fallback(cut), "and so does one it could not settle");
		assert!(Animate::Allowed.accepts_fallback(ANIMATED_GIF), "nothing is withheld here");
	}

	/// A settled walk names the container it read.
	///
	/// That name is what an animating picture is stored under, so it has to be
	/// the container's own rather than whatever the upload declared it to be.
	#[test]
	fn a_settled_walk_names_its_container() {
		assert_eq!(animated_type(ANIMATED_PNG), Some("image/apng"));
		assert_eq!(animated_type(STILL_PNG), None);
	}

	/// An animation cannot be truncated into a still.
	///
	/// A walk that runs out of bytes has not shown the picture to be still, so
	/// every prefix of each of the three is withheld rather than served.
	#[test]
	fn a_picture_cut_short_is_withheld() {
		for animation in [ANIMATED_PNG, ANIMATED_WEBP, ANIMATED_GIF] {
			for len in NAMED_BYTES..animation.len() {
				let prefix = animation.get(..len).unwrap_or(animation);

				assert!(animates(prefix), "a {len} byte prefix was answered as a still");
			}
		}
	}

	/// Nothing that is not one of the three animating formats is withheld.
	///
	/// A JPEG or a text file reaching the classifier must pass, since the rule
	/// exists to withhold animation rather than to police formats.
	#[test]
	fn an_unrecognized_container_animates_nothing() {
		assert!(!animates(b""));
		assert!(!animates(b"\xff\xd8\xff\xe0 jpeg"));
		assert!(!animates(b"not a picture at all"));
	}
}

#[cfg(feature = "media_thumbnail")]
mod animation {
	use std::{io::Cursor, iter::repeat_with};

	use image::{
		AnimationDecoder, Frame, Rgba, RgbaImage,
		codecs::gif::{GifDecoder, GifEncoder},
	};

	use super::{super::thumbnail::encode_frames, scale};

	// Pixels the tests give the encoder to spend, past anything they need.
	const BUDGET: u64 = 1_000_000;

	/// A canvas past the budget is refused before any frame is decoded.
	///
	/// The decoders carry no limits of their own and materialize a whole source
	/// canvas on the first advance, so this thirteen byte header declaring a
	/// 65535 by 65535 picture would otherwise ask for about seventeen gigabytes
	/// before any per-frame cap could apply.
	#[test]
	fn a_canvas_past_the_budget_is_refused() {
		let header = b"GIF89a\xff\xff\xff\xff\x00\x00\x00";

		encode_frames(header, &scale(32, 32), 9, BUDGET).expect_err("refuses");
	}

	/// A red GIF of this many four-by-four frames.
	///
	/// The picture is whatever encodes smallest, since only the frame count and
	/// the dimensions are ever asserted on.
	fn animation(frames: usize) -> Vec<u8> {
		let buffer = RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]));
		let mut content = Vec::new();
		let mut encoder = GifEncoder::new(&mut content);

		encoder
			.encode_frames(repeat_with(|| Frame::new(buffer.clone())).take(frames))
			.expect("encodes");

		drop(encoder);

		content
	}

	fn frames(bytes: &[u8]) -> usize {
		GifDecoder::new(Cursor::new(bytes))
			.expect("decodes")
			.into_frames()
			.count()
	}

	/// A source with more frames than the cap loops short.
	///
	/// Truncating is what the MSC wants over an error, since a client asked for
	/// an animation and a shorter one still answers that.
	#[test]
	fn the_frame_cap_truncates_rather_than_refusing() {
		let thumbnail = encode_frames(&animation(9), &scale(32, 32), 3, BUDGET).expect("encodes");

		assert_eq!(frames(&thumbnail), 3);
	}

	/// A cap the source does not reach leaves every frame in place.
	#[test]
	fn a_shorter_source_keeps_all_of_its_frames() {
		let thumbnail = encode_frames(&animation(3), &scale(32, 32), 9, BUDGET).expect("encodes");

		assert_eq!(frames(&thumbnail), 3);
	}

	/// One frame is no animation, and the caller answers with a still instead.
	#[test]
	fn a_single_frame_is_not_an_animation() {
		encode_frames(&animation(1), &scale(32, 32), 9, BUDGET).expect_err("refuses");
	}

	/// The pixel budget is spent across frames rather than by one of them.
	///
	/// A budget under two frames' worth of source leaves too few to be an
	/// animation, which is the same answer a single-frame source gets.
	#[test]
	fn the_pixel_budget_is_spent_across_frames() {
		encode_frames(&animation(9), &scale(32, 32), 9, 16).expect_err("one frame is refused");
		encode_frames(&animation(9), &scale(32, 32), 9, 48).expect("two frames are an animation");
	}
}

#[cfg(feature = "media_thumbnail")]
mod generate {
	use image::{DynamicImage, RgbImage};

	use super::{super::thumbnail::thumbnail_generate, crop, scale};

	fn blank(width: u32, height: u32) -> DynamicImage {
		DynamicImage::ImageRgb8(RgbImage::new(width, height))
	}

	/// Servers must not upscale under any circumstance. A video's frame reaches
	/// generation without the passthrough guard that spares an image, so the
	/// crop path has to refuse to enlarge on its own.
	#[test]
	fn crop_never_upscales() {
		let thumbnail = thumbnail_generate(&blank(50, 50), &crop(96, 96)).unwrap();

		assert_eq!((thumbnail.width(), thumbnail.height()), (50, 50));
	}

	/// A source larger in one dimension only still must not grow in the other.
	#[test]
	fn crop_never_upscales_one_dimension() {
		let thumbnail = thumbnail_generate(&blank(500, 50), &crop(96, 96)).unwrap();

		assert_eq!((thumbnail.width(), thumbnail.height()), (96, 50));
	}

	/// A crop request inside the source is still honoured exactly.
	#[test]
	fn crop_within_the_source_is_exact() {
		let thumbnail = thumbnail_generate(&blank(500, 300), &crop(96, 96)).unwrap();

		assert_eq!((thumbnail.width(), thumbnail.height()), (96, 96));
	}

	/// The scale path clamps through `Dim::scaled`; pin it so the two branches
	/// cannot drift apart.
	#[test]
	fn scale_never_upscales() {
		let thumbnail = thumbnail_generate(&blank(50, 40), &scale(800, 600)).unwrap();

		assert!(thumbnail.width() <= 50 && thumbnail.height() <= 40);
	}
}

#[cfg(feature = "media_thumbnail")]
mod video {
	use std::{borrow::Cow, path::Path};

	use super::super::video::substitute;

	/// The staged path and the requested size reach the program only through
	/// token substitution, and a token may appear more than once in one
	/// argument.
	#[test]
	fn substitutes_every_token_in_an_argument() {
		let path = Path::new("/tmp/tuwunel-video-Ahk3");
		let arg = substitute("{input}:{width}x{height}:{width}", path, "320", "240");

		assert_eq!(arg, "/tmp/tuwunel-video-Ahk3:320x240:320");
	}

	/// An argument carrying no token is the program's own flag, and must reach
	/// it byte for byte without being copied to do so.
	#[test]
	fn borrows_an_argument_without_a_token() {
		let path = Path::new("/tmp/tuwunel-video-Ahk3");
		let arg = substitute("-frames:v", path, "320", "240");

		assert_eq!(arg, "-frames:v");
		assert!(matches!(arg, Cow::Borrowed(_)), "an untouched argument was copied");
	}
}

#[cfg(all(unix, feature = "media_thumbnail"))]
mod program {
	use std::{env::temp_dir, fs::remove_file, time::Duration};

	use tokio::time::{Instant, sleep, timeout};
	use tuwunel_core::utils::random_string;

	use super::super::video::run;

	const SHELL: &str = "/bin/sh";

	const LIMIT: u64 = 4096;

	/// Every program under test exits at once; the deadline is present only to
	/// keep a hung one from hanging the suite.
	fn deadline() -> Instant { after(Duration::from_secs(30)) }

	fn after(duration: Duration) -> Instant {
		Instant::now()
			.checked_add(duration)
			.expect("a deadline within the epoch")
	}

	/// The frame arrives on standard output, not through a file the program is
	/// told to write.
	#[tokio::test]
	async fn collects_the_frame_from_standard_output() {
		let args = ["-c", "printf frame"];
		let frame = run(SHELL, args, LIMIT, deadline())
			.await
			.expect("a program writing output produces a frame");

		assert_eq!(frame.as_slice(), b"frame");
	}

	/// A program can exit zero having decoded nothing, so an empty output is a
	/// failure rather than a zero-length thumbnail.
	#[tokio::test]
	async fn rejects_a_program_that_writes_no_frame() {
		let args = ["-c", "exit 0"];

		run(SHELL, args, LIMIT, deadline())
			.await
			.expect_err("a program writing nothing produces no frame");
	}

	/// A misconfigured command is diagnosed from the program's own standard
	/// error, which is the only account of why it failed.
	#[tokio::test]
	async fn reports_the_diagnostic_of_a_failing_program() {
		let args = ["-c", "echo Unknown encoder >&2; exit 1"];
		let error = run(SHELL, args, LIMIT, deadline())
			.await
			.expect_err("a non-zero exit is a failure")
			.to_string();

		assert!(error.contains("Unknown encoder"), "{error}");
	}

	/// A program overrunning the deadline is killed along with whatever it
	/// spawned. The wrapper exits at once and leaves the descendant holding
	/// the pipe, which is the shape that orphans work: waiting on the direct
	/// child alone reports the program finished while its decoder runs on.
	#[tokio::test]
	async fn kills_the_group_of_a_wrapper_that_exits() {
		let marker = temp_dir().join(format!("tuwunel-group-{}", random_string(16)));
		let script = format!("(sleep 1; touch {}) &", marker.display());
		let args = ["-c", script.as_str()];

		run(SHELL, args, LIMIT, after(Duration::from_millis(200)))
			.await
			.expect_err("a program past its deadline fails");

		// outlive the descendant's own delay, so that its absence means it was
		// killed rather than merely still sleeping
		sleep(Duration::from_millis(1500)).await;

		let survived = marker.exists();
		remove_file(&marker).ok();

		assert!(!survived, "a descendant outlived the killed group");
	}

	/// Shutdown and a disconnected client both drop the request rather than
	/// expire a deadline, so the group has to die on the dropped future too.
	#[tokio::test]
	async fn kills_the_group_of_a_cancelled_program() {
		let marker = temp_dir().join(format!("tuwunel-cancel-{}", random_string(16)));
		let script = format!("(sleep 1; touch {}) &", marker.display());
		let args = ["-c", script.as_str()];

		// boxed so that dropping the binding drops the future itself, which a
		// stack pin would not
		let mut running = Box::pin(run(SHELL, args, LIMIT, deadline()));

		timeout(Duration::from_millis(200), &mut running)
			.await
			.expect_err("the program should still be running");

		drop(running);
		sleep(Duration::from_millis(1500)).await;

		let survived = marker.exists();
		remove_file(&marker).ok();

		assert!(!survived, "a descendant outlived the cancelled request");
	}

	/// A frame past the limit is refused outright: serving the prefix would
	/// hand the thumbnailer a truncation to fail on, reporting a decode error
	/// for what is really an oversized frame.
	#[tokio::test]
	async fn refuses_a_frame_past_the_limit() {
		let args = ["-c", "printf frame"];
		let error = run(SHELL, args, 3, deadline())
			.await
			.expect_err("five bytes is past a three byte limit")
			.to_string();

		assert!(error.contains("past 3 bytes"), "{error}");
	}

	/// A frame filling the limit exactly is not mistaken for one past it.
	#[tokio::test]
	async fn accepts_a_frame_filling_the_limit() {
		let args = ["-c", "printf frame"];
		let frame = run(SHELL, args, 5, deadline())
			.await
			.expect("five bytes fits a five byte limit");

		assert_eq!(frame.as_slice(), b"frame");
	}
}

#[tokio::test]
#[cfg(disable)] //TODO: fixme
async fn long_file_names_works() {
	use std::path::PathBuf;

	use base64::{Engine as _, engine::general_purpose};

	use super::*;

	struct MockedKVDatabase;

	impl Data for MockedKVDatabase {
		fn create_file_metadata(
			&self,
			_sender_user: Option<&str>,
			mxc: String,
			width: u32,
			height: u32,
			content_disposition: Option<&str>,
			content_type: Option<&str>,
		) -> Result<Vec<u8>> {
			// copied from src/database/key_value/media.rs
			let mut key = mxc.as_bytes().to_vec();
			key.push(0xFF);
			key.extend_from_slice(&width.to_be_bytes());
			key.extend_from_slice(&height.to_be_bytes());
			key.push(0xFF);
			key.extend_from_slice(
				content_disposition
					.as_ref()
					.map(|f| f.as_bytes())
					.unwrap_or_default(),
			);
			key.push(0xFF);
			key.extend_from_slice(
				content_type
					.as_ref()
					.map(|c| c.as_bytes())
					.unwrap_or_default(),
			);

			Ok(key)
		}

		fn delete_file_mxc(&self, _mxc: String) -> Result { todo!() }

		fn search_mxc_metadata_prefix(&self, _mxc: String) -> Result<Vec<Vec<u8>>> { todo!() }

		fn get_all_media_keys(&self) -> Vec<Vec<u8>> { todo!() }

		fn search_file_metadata(
			&self,
			_mxc: String,
			_width: u32,
			_height: u32,
		) -> Result<(Option<String>, Option<String>, Vec<u8>)> {
			todo!()
		}

		fn remove_url_preview(&self, _url: &str) -> Result { todo!() }

		fn set_url_preview(
			&self,
			_url: &str,
			_data: &UrlPreviewData,
			_timestamp: std::time::Duration,
		) -> Result {
			todo!()
		}

		fn get_url_preview(&self, _url: &str) -> Option<UrlPreviewData> { todo!() }
	}

	let db: Arc<MockedKVDatabase> = Arc::new(MockedKVDatabase);
	let mxc = "mxc://example.com/ascERGshawAWawugaAcauga".to_owned();
	let width = 100;
	let height = 100;
	let content_disposition = "attachment; filename=\"this is a very long file name with spaces \
	                           and special characters like äöüß and even emoji like 🦀.png\"";
	let content_type = "image/png";
	let key = db
		.create_file_metadata(
			None,
			mxc,
			width,
			height,
			Some(content_disposition),
			Some(content_type),
		)
		.unwrap();
	let mut r = PathBuf::from("/tmp/media");
	// r.push(base64::encode_config(key, base64::URL_SAFE_NO_PAD));
	// use the sha256 hash of the key as the file name instead of the key itself
	// this is because the base64 encoded key can be longer than 255 characters.
	r.push(general_purpose::URL_SAFE_NO_PAD.encode(<sha2::Sha256 as sha2::Digest>::digest(key)));
	// Check that the file path is not longer than 255 characters
	// (255 is the maximum length of a file path on most file systems)
	assert!(
		r.to_str().unwrap().len() <= 255,
		"File path is too long: {}",
		r.to_str().unwrap().len()
	);
}

/// An upload is refused strictly below the minimum and taken at it, and a
/// minimum of zero refuses nothing however little space is left.
#[test]
fn short_of_space_is_strictly_below_the_minimum() {
	use super::is_short_of_space;

	assert!(is_short_of_space(9, 10));
	assert!(!is_short_of_space(10, 10));
	assert!(!is_short_of_space(11, 10));
	assert!(!is_short_of_space(0, 0));
	assert!(is_short_of_space(u64::MAX - 1, u64::MAX));
}
