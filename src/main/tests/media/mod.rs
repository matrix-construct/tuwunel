//! Media corpus and thumbnail sweep shared by the media regression binaries.
//!
//! Each such binary compiles its own copy of this module, so every item here
//! is used by each of them.
//!
//! The sweep reports what a client can observe (status, content type,
//! disposition, and the decoded shape of the body) and deliberately not the
//! body's length or bytes: an encoder's output size is not stable across
//! architectures, so pinning it would make the snapshot host-specific while
//! adding nothing a client depends on.

use std::{fs::remove_dir_all, io::Cursor, path::PathBuf, time::Duration};

use image::{
	AnimationDecoder, Frames, ImageFormat, ImageReader,
	codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder},
	guess_format,
};
use reqwest::{RequestBuilder, Response};
use serde_json::Value;
use tokio::time::{sleep, timeout};
use tuwunel_core::{
	Result, err,
	ruma::{OwnedUserId, UserId},
};
use tuwunel_service::{Services, users::Register};

/// Password every harness account registers with.
///
/// No binary authenticates through it, since [`register`] installs the device
/// token each request then carries.
const PASSWORD: &str = "tuwunel-test-harness-password";

/// How often the readiness probe retries the listener.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long the readiness probe waits before giving the listener up.
const READY_DEADLINE: Duration = Duration::from_secs(10);

/// The database directory a media test boots against.
///
/// Removed on drop rather than at the end of a test body, so a case that
/// panics leaves no directory behind for the next run to trip over.
pub(crate) struct DatabasePath(pub(crate) PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

/// A source picture, with the content type it is uploaded under.
///
/// The declared type is part of the case rather than derived from the bytes,
/// because a server can only classify what a client tells it: the corpus
/// uploads its APNG as `image/png`, which is how APNG reaches a homeserver in
/// practice.
pub(crate) struct Source {
	/// Corpus-unique name, which also names the row in the report.
	pub(crate) name: &'static str,

	/// The `Content-Type` the upload declares.
	pub(crate) content_type: &'static str,

	/// The file itself.
	pub(crate) bytes: &'static [u8],
}

/// One thumbnail request shape.
///
/// The width and height are what the client asks for, not what it receives:
/// `Dim::normalized` maps every request onto one of a handful of buckets, so
/// a 16x16 ask is answered at 32x32.
pub(crate) struct Ask {
	/// Requested width.
	pub(crate) width: u32,

	/// Requested height.
	pub(crate) height: u32,

	/// Requested resize method.
	pub(crate) method: &'static str,

	/// The MSC2705 `animated` parameter, absent when `None`.
	pub(crate) animated: Option<bool>,
}

/// What one request was answered with.
///
/// Everything here is observable by an ordinary client, which is the point:
/// a case can only assert on what a client could have noticed. The body is
/// carried as a rendered description rather than bytes.
pub(crate) struct Answer {
	/// HTTP status.
	pub(crate) status: u16,

	/// Response `Content-Type`, absent when the response carried none.
	pub(crate) content_type: Option<String>,

	/// Response `Content-Disposition`, absent when the response carried none.
	pub(crate) disposition: Option<String>,

	/// The decoded shape of the body, rendered by [`describe`].
	pub(crate) body: String,
}

/// The corpus every media regression binary uploads.
///
/// Four sizes bracket the `Dim::normalized` buckets: `1x1` upscales at every
/// one and so reaches the passthrough branch, `100x100` straddles them,
/// `1000x800` is downscaled by all of them, and `1200x900` covers the largest
/// request the suite makes, which is past every bucket. The APNG uploads as
/// `image/png`, the one animated shape a content type cannot name, and
/// `anim_as_png.gif` carries GIF bytes under that same declared type, since an
/// upload stores the content type a client sends without checking it against
/// the file. The two truncated entries are the only bodies that decode
/// partway, one failing at the header and one part way through its frames,
/// which is what keeps both failure sentinels honest.
pub(crate) const CORPUS: &[Source] = &[
	Source {
		name: "still_1x1.png",
		content_type: "image/png",
		bytes: include_bytes!("fixtures/still_1x1.png"),
	},
	Source {
		name: "still_100x100.png",
		content_type: "image/png",
		bytes: include_bytes!("fixtures/still_100x100.png"),
	},
	Source {
		name: "still_1000x800.png",
		content_type: "image/png",
		bytes: include_bytes!("fixtures/still_1000x800.png"),
	},
	Source {
		name: "still_100x100.jpg",
		content_type: "image/jpeg",
		bytes: include_bytes!("fixtures/still_100x100.jpg"),
	},
	Source {
		name: "still_100x100.webp",
		content_type: "image/webp",
		bytes: include_bytes!("fixtures/still_100x100.webp"),
	},
	Source {
		name: "still_1000x800.webp",
		content_type: "image/webp",
		bytes: include_bytes!("fixtures/still_1000x800.webp"),
	},
	Source {
		name: "still_1x1.gif",
		content_type: "image/gif",
		bytes: include_bytes!("fixtures/still_1x1.gif"),
	},
	Source {
		name: "anim_100x100.gif",
		content_type: "image/gif",
		bytes: include_bytes!("fixtures/anim_100x100.gif"),
	},
	Source {
		name: "anim_1000x800.gif",
		content_type: "image/gif",
		bytes: include_bytes!("fixtures/anim_1000x800.gif"),
	},
	Source {
		name: "anim_1200x900.gif",
		content_type: "image/gif",
		bytes: include_bytes!("fixtures/anim_1200x900.gif"),
	},
	Source {
		name: "anim_100x100.webp",
		content_type: "image/webp",
		bytes: include_bytes!("fixtures/anim_100x100.webp"),
	},
	Source {
		name: "anim_100x100.apng",
		content_type: "image/png",
		bytes: include_bytes!("fixtures/anim_100x100.apng"),
	},
	Source {
		name: "anim_as_png.gif",
		content_type: "image/png",
		bytes: include_bytes!("fixtures/anim_100x100.gif"),
	},
	Source {
		name: "notimage.txt",
		content_type: "text/plain",
		bytes: include_bytes!("fixtures/notimage.txt"),
	},
	Source {
		name: "truncated.png",
		content_type: "image/png",
		bytes: include_bytes!("fixtures/truncated.png"),
	},
	Source {
		name: "truncated.gif",
		content_type: "image/gif",
		bytes: include_bytes!("fixtures/truncated.gif"),
	},
];

/// One requested size per `Dim::normalized` outcome, plus one below them all.
///
/// The first rounds up to the smallest bucket, the next five land on one
/// bucket each, and the last exceeds every bucket and so reaches the
/// zero-dimension sentinel, which stands for the original file rather than a
/// size.
pub(crate) const SIZES: &[(u32, u32)] =
	&[(16, 16), (32, 32), (96, 96), (320, 240), (640, 480), (800, 600), (1024, 768)];

/// The three states MSC2705 gives the `animated` parameter.
///
/// Absent and `false` are distinct on the wire but carry the same posture,
/// that the response must not animate; only `true` permits it. Both are swept
/// because a handler can easily honor one and drop the other.
pub(crate) const ANIMATED: &[Option<bool>] = &[None, Some(false), Some(true)];

/// Every request shape the sweep covers, in report order.
///
/// `Dim::normalized` replaces the requested method with its bucket's own
/// before any lookup, so the requested method cannot reach the answer and is
/// pinned rather than swept.
pub(crate) fn asks() -> impl Iterator<Item = Ask> {
	SIZES.iter().flat_map(|&(width, height)| {
		ANIMATED
			.iter()
			.map(move |&animated| Ask { width, height, method: "scale", animated })
	})
}

/// Wait for the listener to answer, which the boot does not itself await.
///
/// A request issued before then fails to connect, so the probe retries until
/// the versions endpoint answers or the deadline passes.
pub(crate) async fn wait_until_ready(services: &Services, base: &str) -> Result {
	let url = format!("{base}/_matrix/client/versions");

	let reachable = async || {
		services
			.client
			.clients
			.default
			.get(&url)
			.send()
			.await
			.is_ok()
	};

	let probe = timeout(READY_DEADLINE, async {
		while !reachable().await {
			sleep(POLL_INTERVAL).await;
		}
	});

	probe
		.await
		.map_err(|_| err!("server listener did not become ready"))
}

/// Register a local user and give it a device holding `token`.
///
/// The device is created directly rather than through the client API so the
/// token is known up front and every later request can carry it.
pub(crate) async fn register(
	services: &Services,
	localpart: &str,
	token: &str,
) -> Result<OwnedUserId> {
	let user_id = UserId::parse_with_server_name(localpart, services.globals.server_name())?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some(PASSWORD),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	Ok(user_id)
}

/// Upload one source under a fresh media id and return that id.
///
/// Every case takes its own media, because the variant a request stores is
/// visible to every later request for the same picture at the same size.
/// `user_id` masquerades the upload, which is how an appservice uploads on
/// behalf of a ghost it claims; the server rejects one it does not.
pub(crate) async fn upload(
	services: &Services,
	base: &str,
	token: &str,
	source: &Source,
	user_id: Option<&str>,
) -> Result<String> {
	let request = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/media/v3/upload"))
		.bearer_auth(token)
		.header("content-type", source.content_type)
		.body(source.bytes);

	let request = user_id
		.into_iter()
		.fold(request, |request, user_id| request.query(&[("user_id", user_id)]));

	let response: Value = request
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	let uri = response
		.get("content_uri")
		.and_then(Value::as_str)
		.ok_or_else(|| err!("upload answered without a content_uri: {response}"))?;

	uri.rsplit_once('/')
		.map(|(_, media_id)| media_id.to_owned())
		.ok_or_else(|| err!("content_uri names no media id: {uri}"))
}

/// Issue one thumbnail request and observe what it is answered with.
///
/// An absent `token` addresses the unauthenticated surface, `user_id`
/// masquerades the read the way an appservice reads for a ghost, and an absent
/// `Ask::animated` sends no `animated` parameter at all, which MSC2705 gives a
/// meaning of its own rather than treating as a default. Only a 200 carries a
/// body worth describing, so anything else reports `-` rather than decoding
/// an error document as a picture.
pub(crate) async fn thumbnail(
	services: &Services,
	url: &str,
	token: Option<&str>,
	user_id: Option<&str>,
	ask: &Ask,
) -> Result<Answer> {
	let request = services
		.client
		.clients
		.default
		.get(url)
		.query(&[("width", ask.width), ("height", ask.height)])
		.query(&[("method", ask.method)]);

	let request = ask
		.animated
		.into_iter()
		.fold(request, |request, animated| request.query(&[("animated", animated)]));

	let request = user_id
		.into_iter()
		.fold(request, |request, user_id| request.query(&[("user_id", user_id)]));

	let response = authorize(request, token).send().await?;
	let status = response.status().as_u16();
	let content_type = header(&response, "content-type").map(ToOwned::to_owned);
	let disposition = header(&response, "content-disposition").map(ToOwned::to_owned);
	let bytes = response.bytes().await?;

	let body = match status {
		| 200 => describe(&bytes),
		| _ => "-".to_owned(),
	};

	Ok(Answer { status, content_type, disposition, body })
}

/// Carry a bearer token on a request when one is given.
///
/// An absent token leaves the request as it was, which is how the
/// unauthenticated surfaces are addressed.
pub(crate) fn authorize(request: RequestBuilder, token: Option<&str>) -> RequestBuilder {
	token
		.into_iter()
		.fold(request, RequestBuilder::bearer_auth)
}

/// One response header as text, absent when missing or not text.
///
/// Borrowed from the response, so each caller keeps it in the form its own
/// record wants.
pub(crate) fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
	response
		.headers()
		.get(name)
		.and_then(|value| value.to_str().ok())
}

/// Render the decoded shape of a response body.
///
/// Reports the format, dimensions and frame count, so an animated answer is
/// told from a still one by what it contains rather than by what it claims.
/// Four sentinels stand for bodies that do not fully decode: `empty`,
/// `not-an-image` for bytes no decoder claims, `<format> undecodable` for a
/// header that parses no further, and `frames-undecodable` for a container
/// that should yield frames and does not.
pub(crate) fn describe(bytes: &[u8]) -> String {
	if bytes.is_empty() {
		return "empty".to_owned();
	}

	let Ok(format) = guess_format(bytes) else {
		return "not-an-image".to_owned();
	};

	let Some((width, height)) = dimensions(bytes) else {
		return format!("{format:?} undecodable");
	};

	let Some(frames) = frames(format, bytes) else {
		return format!("{format:?} {width}x{height} frames-undecodable");
	};

	format!("{format:?} {width}x{height} f{frames}")
}

/// Dimensions read from the body's own header.
///
/// Header-only for every format, so a body is never decoded to learn its
/// size. A body that does not parse is reported rather than raised, since the
/// corpus carries a truncated file on purpose.
fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
	ImageReader::new(Cursor::new(bytes))
		.with_guessed_format()
		.ok()?
		.into_dimensions()
		.ok()
}

/// Frames the body carries, counting a still picture as one.
///
/// `None` is reserved for a container that should yield frames and does not,
/// so a corrupt animation cannot read as a correctly generated still. A body
/// that simply is not an animation is one frame, not a failure.
fn frames(format: ImageFormat, bytes: &[u8]) -> Option<usize> {
	let cursor = Cursor::new(bytes);

	match format {
		| ImageFormat::Gif => GifDecoder::new(cursor)
			.ok()
			.map(AnimationDecoder::into_frames)
			.and_then(count_frames),
		| ImageFormat::WebP => match WebPDecoder::new(cursor) {
			| Err(_) => None,
			| Ok(decoder) if !decoder.has_animation() => Some(1),
			| Ok(decoder) => count_frames(decoder.into_frames()),
		},
		| ImageFormat::Png => match PngDecoder::new(cursor) {
			| Err(_) => None,
			| Ok(decoder) if !decoder.is_apng().unwrap_or(false) => Some(1),
			| Ok(decoder) => decoder
				.apng()
				.ok()
				.map(AnimationDecoder::into_frames)
				.and_then(count_frames),
		},
		| _ => Some(1),
	}
}

/// Frames in a decoded sequence, or `None` if any of them fails to decode.
///
/// Takes the sequence rather than the decoder that produced it, so the three
/// callers share one body without this needing a lifetime of its own. The
/// frames are counted rather than kept, so a long animation is walked without
/// retaining it, and one bad frame invalidates the count instead of silently
/// shortening it.
fn count_frames(mut frames: Frames<'_>) -> Option<usize> {
	frames.try_fold(0_usize, |count, frame| frame.ok().and_then(|_| count.checked_add(1)))
}

/// Render one swept case as a report line.
///
/// The columns are padded to a fixed width so the report reads as a table and
/// a diff between two runs lines up field by field. An absent header prints as
/// `-`, which no real header value collides with. `surface` is the caller's to
/// name, and two binaries reporting the same endpoint must spell it the same
/// way for their snapshots to be read side by side.
pub(crate) fn row(surface: &str, source: &Source, ask: &Ask, answer: &Answer) -> String {
	let animated = match ask.animated {
		| None => "absent",
		| Some(true) => "true",
		| Some(false) => "false",
	};

	let content_type = answer.content_type.as_deref().unwrap_or("-");
	let disposition = answer.disposition.as_deref().unwrap_or("-");

	format!(
		"{surface:<7} {name:<19} {width:>4}x{height:<4} {method:<5} anim={animated:<6} -> \
		 {status:<3} {content_type:<24} {body:<22} {disposition}",
		name = source.name,
		width = ask.width,
		height = ask.height,
		method = ask.method,
		status = answer.status,
		body = answer.body,
	)
}
