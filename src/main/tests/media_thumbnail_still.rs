#![cfg(test)]
#![cfg(feature = "media_thumbnail")]

use std::{fs::remove_dir_all, path::PathBuf};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result, ruma::Mxc};
use tuwunel_service::{
	Services,
	media::{Animate, Dim},
};

/// A one-by-one GIF carrying two frames.
///
/// Both properties are load-bearing. Every bucket is larger, so an in-bucket
/// request upscales and reaches the passthrough branch, and the second frame
/// is what puts withholding under test rather than the content type.
const GIF: &[u8] = &[
	0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
	0x00, 0xFF, 0xFF, 0xFF, 0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00,
	0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x44, 0x00, 0x21, 0xF9, 0x04, 0x01,
	0x00, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02,
	0x01, 0x44, 0x00, 0x3B,
];

/// One media per case, since the variant a request stores is visible to every
/// later request for the same picture.
///
/// Sharing one would make each case's answer depend on the cases that ran
/// before it rather than on the request under test.
const STILL_ID: &str = "stillthumbnailregressionmediaid0";
const ANIMATED_ID: &str = "animatedthumbnailregressionmedia";
const MISLABELED_ID: &str = "mislabeledthumbnailrowregression";

const GIF_TYPE: &str = "image/gif";

const PNG_TYPE: &str = "image/png";

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

/// A request forbidding animation is never answered with an animated file.
///
/// The same picture still animates for a request that permits it, so the rule
/// is shown to withhold rather than to have broken generation outright.
#[test]
fn still_request_is_never_answered_with_an_animated_file() -> Result {
	let db_path = DatabasePath(Args::test_database_path("media-thumbnail-still"));

	let mut args = Args::default_test(&["fresh", "cleanup"])
		.with_option(format!("database_path={:?}", db_path.0));

	args.maintenance = true;

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = exercise(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

async fn exercise(services: &Services) -> Result {
	let still = upload(services, STILL_ID).await?;
	let animated = upload(services, ANIMATED_ID).await?;

	// an absent parameter forbids animation exactly as an explicit false does,
	// so both must re-encode rather than pass the original through
	for animate in [Animate::Never, Animate::from(None), Animate::from(Some(false))] {
		let content_type = thumbnail_type(services, &still, 32, 32, animate).await?;

		if content_type != PNG_TYPE {
			return Err!("{animate:?} was answered with {content_type}, expected {PNG_TYPE}");
		}
	}

	// the same picture, untouched by a still request, still animates for one
	let content_type = thumbnail_type(services, &animated, 32, 32, Animate::Allowed).await?;

	if content_type != GIF_TYPE {
		return Err!("animated request was answered with {content_type}, expected {GIF_TYPE}");
	}

	// an animated request accepts any stored variant, so the still a forbidding
	// request cached answers permitting ones until an encoder can beat it
	let content_type = thumbnail_type(services, &still, 32, 32, Animate::Allowed).await?;

	if content_type != PNG_TYPE {
		return Err!("expected the stored still to answer, got {content_type}");
	}

	oversized_request_stands_in_for_the_original(services, &still).await?;
	a_row_is_judged_by_its_picture(services).await
}

async fn upload<'a>(services: &'a Services, media_id: &'a str) -> Result<Mxc<'a>> {
	let mxc = Mxc {
		server_name: services.globals.server_name(),
		media_id,
	};

	services
		.media
		.create(&mxc, None, None, Some(GIF_TYPE), GIF)
		.await?;

	Ok(mxc)
}

async fn thumbnail_type(
	services: &Services,
	mxc: &Mxc<'_>,
	width: u32,
	height: u32,
	animate: Animate,
) -> Result<String> {
	services
		.media
		.get_stored_thumbnail(mxc, &Dim::new(width, height, None), animate)
		.await
		.map(|media| media.content_type.unwrap_or_default())
}

/// A request past the largest bucket is answered out of the original file.
///
/// The sentinel every such request normalizes to is the key the original is
/// stored under, so it cannot also hold a still. A request that may not have
/// the original is answered by a still carrying the original's own dimensions,
/// rather than being refused or encoded at the sentinel's zero dimensions.
async fn oversized_request_stands_in_for_the_original(
	services: &Services,
	mxc: &Mxc<'_>,
) -> Result {
	let oversized = Dim::new(1200, 900, None);

	let permitted = services
		.media
		.get_stored_thumbnail(mxc, &oversized, Animate::Allowed)
		.await?;

	if permitted.content != GIF {
		return Err!("a permitting oversized request did not return the original file");
	}

	let withheld = services
		.media
		.get_stored_thumbnail(mxc, &oversized, Animate::Never)
		.await?;

	let content_type = withheld
		.content_type
		.as_deref()
		.unwrap_or_default();

	match content_type == PNG_TYPE {
		| true => Ok(()),
		| false => Err!("a forbidding oversized request was answered with {content_type}"),
	}
}

/// A stored row that animates is withheld whatever type it was stored under.
///
/// Nothing local produces such a row, since every thumbnail this generates is
/// a still PNG. A peer's APNG arrives labelled `image/png` honestly and is
/// cached under that, and the row is then all a later request has to go on.
async fn a_row_is_judged_by_its_picture(services: &Services) -> Result {
	let mxc = Mxc {
		server_name: services.globals.server_name(),
		media_id: MISLABELED_ID,
	};

	let dim = Dim::new(32, 32, None);

	services
		.media
		.upload_thumbnail(&mxc, None, Some(PNG_TYPE), &dim, GIF)
		.await?;

	// the permitting request runs first, which is the order that puts an
	// animation in the cache for the forbidding ones behind it to meet
	let served = services
		.media
		.get_stored_thumbnail(&mxc, &dim, Animate::Allowed)
		.await?;

	if served.content != GIF {
		return Err!("a permitting request was refused the animation the row holds");
	}

	let withheld = services
		.media
		.get_stored_thumbnail(&mxc, &dim, Animate::Never)
		.await?;

	// the type is `image/png` either way, so the body is what tells them apart
	match withheld.content == GIF {
		| true => Err!("a still request was answered with the row's own animated picture"),
		| false => Ok(()),
	}
}
