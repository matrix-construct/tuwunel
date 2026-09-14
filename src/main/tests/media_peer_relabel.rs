#![cfg(test)]
#![cfg(feature = "media_thumbnail")]

//! What a peer's picture is cached as, and how often it is asked for.
//!
//! A server federating with itself only ever meets its own honest labels and
//! its own cached rows, so the snapshot harness cannot reach either case here:
//! a peer answering with a picture whose bytes contradict the type it declares,
//! which is how an APNG ordinarily arrives, and a request past every bucket,
//! which names the original file rather than a size to encode at.
//!
//! The peer answers each federation media endpoint in the shape that endpoint
//! specifies, the authenticated one in `multipart/mixed` and the legacy one as
//! a bare body under a content type header, and counts what it was asked for,
//! since a request that stops recurring is visible in nothing else.

use std::{
	fs::remove_dir_all,
	net::TcpListener,
	path::PathBuf,
	sync::{
		Arc, Mutex,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use axum::{
	Router,
	extract::State,
	http::{Uri, header::CONTENT_TYPE},
	response::IntoResponse,
	routing::any,
};
use axum_server::{from_tcp_rustls, tls_rustls::RustlsConfig};
use tokio::spawn;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err,
	ruma::{Mxc, OwnedUserId, ServerName, UserId},
};
use tuwunel_service::{
	Services,
	media::{Animate, Dim, Media},
};

/// A one-by-one GIF carrying two frames.
///
/// The second image descriptor is what settles the walk on an animation, and
/// so what the peer's declared type contradicts.
const GIF: &[u8] = &[
	0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
	0x00, 0xFF, 0xFF, 0xFF, 0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00,
	0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x44, 0x00, 0x21, 0xF9, 0x04, 0x01,
	0x00, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02,
	0x01, 0x44, 0x00, 0x3B,
];

/// What the peer claims its picture to be.
///
/// A real APNG arrives under exactly this type, honestly, which is what makes
/// a declared type worth contradicting rather than trusting.
const DECLARED: &str = "image/png";

/// What a still stood in for an animation is encoded as.
///
/// Every thumbnail this generates is a PNG, so a forbidding request answered
/// with one is how the stand-in is told apart from the peer's own picture.
const STILL: &str = "image/png";

/// What the walk settles on, and so what the stored row has to carry.
///
/// The two content types differing is the whole result, so this is the half
/// the assertion is written against.
const SETTLED: &str = "image/gif";

/// Separator for the peer's `multipart/mixed` answer.
///
/// The value is arbitrary except that it must not occur in the picture it
/// separates, which no run of letters can.
const BOUNDARY: &str = "peerstubmultipartboundary";

/// One media per endpoint, since the row a fetch stores answers later lookups.
///
/// Sharing one would let whichever case ran first satisfy the other from cache,
/// leaving the second endpoint's own write untested.
const MEDIA_ID: &str = "peermislabelledthumbnailmediaid0";

const LEGACY_MEDIA_ID: &str = "peermislabelledlegacythumbnailid";

const LEGACY_BUCKET_MEDIA_ID: &str = "peerlegacybucketthumbnailid00000";

const LEGACY_OVERSIZED_MEDIA_ID: &str = "peerlegacyoversizedthumbnailid00";

const LEGACY_WITHHELD_MEDIA_ID: &str = "peerlegacywithheldmediaid0000000";

const OVERSIZED_MEDIA_ID: &str = "peeroversizedthumbnailmediaid000";

const WITHHELD_MEDIA_ID: &str = "peeroversizedwithheldmediaid0000";

const BUCKET_MEDIA_ID: &str = "peerbucketthumbnailmediaid000000";

/// Media the peer answers by pointing at a location rather than sending it.
///
/// A redirected object is filed by nobody, so nothing walks the picture on the
/// way through and the gate that withholds an animation has to walk it itself.
/// Every other exercise here reaches the arm where filing the row settled it.
const LOCATION_MEDIA_ID: &str = "peerredirectedthumbnailmediaid00";

/// Path the peer's redirect points at.
///
/// The stub's fallback serves it like any other request, which is what a real
/// peer's separate media host would do, and it carries no download segment so
/// the tallies read it as neither an original nor a thumbnail.
const REDIRECT: &str = "/redirected-media";

/// Requests the peer has answered.
///
/// A request past every bucket has to stop reaching the peer once the original
/// is cached, and nothing about the answer itself would show that; only the
/// count the peer keeps can.
static ANSWERED: AtomicUsize = AtomicUsize::new(0);

/// Requests the peer has answered for an original rather than a thumbnail.
///
/// Convergence alone cannot tell the two apart, since a thumbnail cached at the
/// sentinel key converges just as well; only which endpoint was asked shows
/// that a request naming the original file went after one.
static DOWNLOADED: AtomicUsize = AtomicUsize::new(0);

/// Path segment a media download endpoint carries, on either API.
///
/// Both APIs spell the segment identically and neither thumbnail endpoint
/// carries it, so one match tells an original apart from a thumbnail whichever
/// half of the stub answered.
const DOWNLOAD: &str = "/download/";

/// Query the peer was last asked with.
///
/// The dimension reaching the peer is the half of this that a converging cache
/// cannot show: filing under the bucket while still asking at the size the
/// client named would converge just as well and be the wrong fix.
static ASKED_QUERY: Mutex<String> = Mutex::new(String::new());

const CERTIFICATE: &str = "../../nix/pkgs/complement/certificate.crt";

const PRIVATE_KEY: &str = "../../nix/pkgs/complement/private_key.key";

const TIMEOUT: Duration = Duration::from_secs(10);

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

/// A peer's animation declared a still is cached under the type its bytes name.
///
/// The answer this request receives is the peer's own, declared type included,
/// so the correction is only observable in the row left behind. Reading that
/// row back is therefore the assertion, and the two content types differing is
/// the whole result.
#[test]
fn a_peers_mislabelled_thumbnail_is_cached_under_its_container() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let address = format!("127.0.0.1:{}", listener.local_addr()?.port());
	let peer = ServerName::parse(&address).map_err(|e| err!("peer server name: {e}"))?;

	// tokio refuses to adopt a blocking socket, and the panic it raises reaches
	// the caller only as a peer that never answered
	listener.set_nonblocking(true)?;

	let db_path = DatabasePath(Args::test_database_path("media-peer-relabel"));

	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let certificate = manifest.join(CERTIFICATE);
	let private_key = manifest.join(PRIVATE_KEY);

	let args = [
		format!("database_path={:?}", db_path.0),
		"allow_invalid_tls_certificates=true".to_owned(),
		"ip_range_denylist=[]".to_owned(),
		"federation_loopback=true".to_owned(),
		"log=\"error\"".to_owned(),
	]
	.into_iter()
	.fold(Args::default_test(&["fresh", "cleanup"]), Args::with_option);

	// the option form of this one is refused, so the field is the only way in
	let args = Args { maintenance: true, ..args };

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let stub = spawn(serve_peer(listener, address, certificate, private_key));
		let outcome = exercises(&services, &peer).await;

		// a stub that never started reaches the caller only as a peer that did
		// not answer, so its own error is reported in place of that symptom
		let outcome = match stub.is_finished() {
			| true => stub
				.await
				.unwrap_or_else(|error| Err(err!("the peer stub panicked: {error}")))
				.and(outcome),
			| false => {
				stub.abort();
				outcome
			},
		};

		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

/// Answers every federation request with the same mislabelled picture.
///
/// The peer is reachable only because its name is the literal address it
/// listens on, which federation resolves without a lookup. Nothing here
/// verifies the request's signature, since what is under test is what the
/// answer is cached as rather than how it was asked for.
async fn serve_peer(
	listener: TcpListener,
	address: String,
	certificate: PathBuf,
	private_key: PathBuf,
) -> Result {
	let config = RustlsConfig::from_pem_file(certificate, private_key).await?;
	let app = Router::new()
		.route("/_matrix/federation/{*rest}", any(authenticated))
		.fallback(any(legacy))
		.with_state(Arc::from(address));

	from_tcp_rustls(listener, config)?
		.serve(app.into_make_service())
		.await?;

	Ok(())
}

/// The `multipart/mixed` body an authenticated media answer carries.
///
/// The parts and their separators are the shape the client half of the API
/// writes, so a body assembled any other way would fail to parse before
/// reaching anything this covers. This is the half that counts its requests,
/// since the authenticated endpoints are the ones a repeat would return to.
async fn authenticated(uri: Uri, State(address): State<Arc<str>>) -> impl IntoResponse {
	record(&uri);

	let body = match uri.path().contains(LOCATION_MEDIA_ID) {
		| true => redirect_body(&address),
		| false => picture_body(),
	};

	([(CONTENT_TYPE, format!("multipart/mixed; boundary={BOUNDARY}"))], body)
}

/// The envelope carrying the picture itself.
///
/// The parts and their separators are the shape the client half of the API
/// writes, so a body assembled any other way would fail to parse before
/// reaching anything this covers.
fn picture_body() -> Vec<u8> {
	let head = format!(
		"\r\n--{BOUNDARY}\r\ncontent-type: \
		 application/json\r\n\r\n{{}}\r\n--{BOUNDARY}\r\ncontent-type: {DECLARED}\r\n\r\n"
	);

	[
		head.as_bytes(),
		GIF,
		b"\r\n--".as_slice(),
		BOUNDARY.as_bytes(),
		b"--".as_slice(),
	]
	.concat()
}

/// The same envelope carrying a location in place of the picture.
///
/// The second part is a bare `location` header and no body, which is the shape
/// the client half parses as a redirect. The address is the stub's own, handed
/// in because this side of the request carries no host to recover it from: the
/// federation client speaks HTTP/2, whose authority is a pseudo-header.
fn redirect_body(address: &str) -> Vec<u8> {
	format!(
		"\r\n--{BOUNDARY}\r\ncontent-type: \
		 application/json\r\n\r\n{{}}\r\n--{BOUNDARY}\r\nlocation: \
		 https://{address}{REDIRECT}\r\n\r\n\r\n--{BOUNDARY}--"
	)
	.into_bytes()
}

/// The bare body a legacy media answer carries.
///
/// The unauthenticated endpoint predates the multipart envelope, so the
/// picture is the whole body and the type it is declared under is a header.
async fn legacy(uri: Uri) -> impl IntoResponse {
	record(&uri);

	([(CONTENT_TYPE, DECLARED)], GIF)
}

/// Records what the peer was asked for, whichever endpoint answered it.
///
/// The two media APIs differ in the shape of an answer but not in what a
/// request has to prove, so one set of tallies serves both and an exercise
/// reads its own delta against a baseline it takes first.
fn record(uri: &Uri) {
	ANSWERED.fetch_add(1, Ordering::Relaxed);

	if uri.path().contains(DOWNLOAD) {
		DOWNLOADED.fetch_add(1, Ordering::Relaxed);
	}

	if let Ok(mut asked) = ASKED_QUERY.lock() {
		asked.clear();
		asked.push_str(uri.query().unwrap_or_default());
	}
}

/// Every exercise in order, stopping at the first one that fails.
///
/// A leg that runs after a failure cannot change the verdict, and each is a
/// federation fetch carrying the full timeout, so a stub that has died would
/// otherwise cost one timeout per remaining leg before the real error is
/// reported.
async fn exercises(services: &Services, peer: &ServerName) -> Result {
	exercise_authenticated(services, peer).await?;
	exercise_legacy(services, peer).await?;
	exercise_legacy_bucket(services, peer).await?;
	exercise_legacy_oversized(services, peer).await?;
	exercise_legacy_withheld(services, peer).await?;
	exercise_oversized(services, peer).await?;
	exercise_withheld(services, peer).await?;
	exercise_location(services, peer).await?;
	exercise_bucket(services, peer).await
}

async fn exercise_authenticated(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc { server_name: peer, media_id: MEDIA_ID };

	let dim = Dim::new(96, 96, None);
	let user = fixture_user(services)?;

	let fetched = services
		.media
		.get_or_fetch_thumbnail(&mxc, &dim, Animate::Allowed, TIMEOUT, &user)
		.await?;

	if fetched.content != GIF {
		return Err!("the peer's own picture did not reach the caller");
	}

	// the answer is relayed as the peer sent it, so a correction here would be
	// a different change than the one under test
	let relayed = fetched
		.content_type
		.as_deref()
		.unwrap_or_default();

	if relayed != DECLARED {
		return Err!("the relayed answer was labelled {relayed}, expected {DECLARED}");
	}

	let stored = services
		.media
		.get_stored_thumbnail(&mxc, &dim, Animate::Allowed)
		.await?;

	let cached = stored.content_type.as_deref().unwrap_or_default();

	if cached != SETTLED {
		return Err!("the cached row was stored as {cached}, expected {SETTLED}");
	}

	Ok(())
}

/// Requests the peer has answered since a baseline.
///
/// The tallies are process-wide and every exercise adds to them, so a count
/// means nothing except against a baseline its own caller took first.
fn asked(before: usize) -> usize {
	ANSWERED
		.load(Ordering::Relaxed)
		.saturating_sub(before)
}

/// Fetches twice, answering what the first fetch returned.
///
/// A row filed where the lookup reads it is what stops the second fetch
/// reaching the peer, so every exercise wants both halves asserted and only
/// the picture differs between them.
async fn fetched_once(
	services: &Services,
	mxc: &Mxc<'_>,
	dim: &Dim,
	animate: Animate,
	user: &UserId,
) -> Result<Media> {
	let before = ANSWERED.load(Ordering::Relaxed);

	let fetch = || {
		services
			.media
			.get_or_fetch_thumbnail(mxc, dim, animate, TIMEOUT, user)
	};

	let fetched = fetch().await?;

	if asked(before) != 1 {
		return Err!("the first request reached the peer {} times, expected 1", asked(before));
	}

	fetch().await?;

	if asked(before) != 1 {
		return Err!("the repeat went back to the peer, {} requests in total", asked(before));
	}

	Ok(fetched)
}

/// The local user every exercise fetches as.
fn fixture_user(services: &Services) -> Result<OwnedUserId> {
	UserId::parse_with_server_name("peerfixture", services.globals.server_name())
		.map_err(Into::into)
}

/// The same claim over the legacy endpoint, which is a second transport.
///
/// The two media APIs differ in how an answer is framed rather than in what
/// files it, since both store through the one handler. What this covers is the
/// bare-body shape reaching that handler, so the row left behind is the whole
/// assertion.
async fn exercise_legacy(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc {
		server_name: peer,
		media_id: LEGACY_MEDIA_ID,
	};

	let dim = Dim::new(96, 96, None);

	services
		.media
		.fetch_remote_thumbnail_legacy(&mxc, TIMEOUT, &dim, Animate::Allowed)
		.await?;

	// the 96x96 rung crops where the request asked to scale, and the row key
	// holds no method, so what shape the peer returns has to be ours to choose
	asked_with(96, 96, "crop")?;

	let stored = services
		.media
		.get_stored_thumbnail(&mxc, &dim, Animate::Allowed)
		.await?;

	let cached = stored.content_type.as_deref().unwrap_or_default();

	if cached != SETTLED {
		return Err!("the legacy row was stored as {cached}, expected {SETTLED}");
	}

	Ok(())
}

/// The legacy endpoint asks at the bucket it will file the answer under.
///
/// This path reaches the peer only once the route's own lookup has missed, so
/// a row filed where that lookup cannot read it is a fetch on every request
/// rather than a slow first one. Asking at the size the client named did
/// exactly that, and the lookup succeeding afterwards is what says it no
/// longer does.
async fn exercise_legacy_bucket(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc {
		server_name: peer,
		media_id: LEGACY_BUCKET_MEDIA_ID,
	};

	// 100x100 takes the 320x240 bucket, so raw and normalized differ
	let dim = Dim::new(100, 100, None);

	services
		.media
		.fetch_remote_thumbnail_legacy(&mxc, TIMEOUT, &dim, Animate::Allowed)
		.await?;

	asked_with(320, 240, "scale")?;

	services
		.media
		.get_stored_thumbnail(&mxc, &dim, Animate::Allowed)
		.await
		.map(drop)
}

/// Fails unless the peer's last request named this bucket.
///
/// A lookup finding the row afterwards says the ask and the file agreed,
/// never which one they agreed on, so this is what separates asking at the
/// bucket from filing under it while still asking at the size a client named.
/// The method is asserted beside the size because the row key holds neither.
fn asked_with(width: u32, height: u32, method: &str) -> Result {
	let Ok(asked) = ASKED_QUERY.lock() else {
		return Err!("the query the peer was asked with is poisoned");
	};

	let named =
		[format!("width={width}"), format!("height={height}"), format!("method={method}")]
			.iter()
			.all(|term| asked.contains(term));

	named.then_some(()).ok_or_else(|| {
		err!("the peer was asked with {:?}, expected {method} {width}x{height}", *asked)
	})
}

/// The legacy endpoint for a request that forbids animation, which is most.
///
/// An absent parameter forbids exactly as an explicit false does, so this is
/// the ordinary shape rather than a corner, and it is the only one reaching
/// the re-encode arm through this transport. The peer answers every request
/// with an animation, so the still it is served proves the repair ran.
async fn exercise_legacy_withheld(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc {
		server_name: peer,
		media_id: LEGACY_WITHHELD_MEDIA_ID,
	};

	let dim = Dim::new(96, 96, None);

	let fetched = services
		.media
		.fetch_remote_thumbnail_legacy(&mxc, TIMEOUT, &dim, Animate::from(None))
		.await?;

	stood_in_for_the_animation(&fetched, "legacy")
}

/// A legacy request past every bucket names the original, and fetches it.
///
/// There is no dimension to ask a peer to thumbnail at once a request
/// normalizes to the sentinel, so this leg asked for one at the raw size and
/// filed the answer where nothing reads. Which endpoint the peer saw is the
/// half a lookup cannot show, since a thumbnail cached at the sentinel key
/// would satisfy it too.
async fn exercise_legacy_oversized(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc {
		server_name: peer,
		media_id: LEGACY_OVERSIZED_MEDIA_ID,
	};

	let dim = Dim::new(1024, 768, None);
	let before = DOWNLOADED.load(Ordering::Relaxed);

	services
		.media
		.fetch_remote_thumbnail_legacy(&mxc, TIMEOUT, &dim, Animate::Allowed)
		.await?;

	let downloads = downloaded(before);

	if downloads != 1 {
		return Err!("the legacy sentinel asked for an original {downloads} times, expected 1");
	}

	services
		.media
		.get_stored_thumbnail(&mxc, &dim, Animate::Allowed)
		.await
		.map(drop)
}

/// Originals the peer has handed over since a baseline.
///
/// Convergence alone cannot say an original was what a request went after, so
/// this counts which endpoint answered rather than the outcome, against a
/// baseline its own caller took first.
fn downloaded(before: usize) -> usize {
	DOWNLOADED
		.load(Ordering::Relaxed)
		.saturating_sub(before)
}

/// A request past every bucket fetches the original, and then stops asking.
///
/// The sentinel every such request normalizes to names the original file, so
/// asking the peer to thumbnail at it stored a row no later lookup could find
/// and every repeat went back over federation. Fetching the original instead
/// leaves the row where the lookup reads it, which is what the second request
/// proves by never reaching the peer.
async fn exercise_oversized(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc {
		server_name: peer,
		media_id: OVERSIZED_MEDIA_ID,
	};

	let dim = Dim::new(1024, 768, None);
	let user = fixture_user(services)?;
	let before = DOWNLOADED.load(Ordering::Relaxed);
	let fetched = fetched_once(services, &mxc, &dim, Animate::Allowed, &user).await?;

	if fetched.content != GIF {
		return Err!("the original did not reach the caller");
	}

	// converging proves a row was filed, not that the original was what was
	// asked for; a thumbnail cached at the sentinel key would converge too
	let downloads = downloaded(before);

	match downloads == 1 {
		| true => Ok(()),
		| false => Err!("the sentinel asked for an original {downloads} times, expected 1"),
	}
}

/// The same fetch for a request that forbids animation, which is most of them.
///
/// An absent parameter forbids exactly as an explicit false does, so this is
/// the ordinary shape rather than a corner: the original is fetched once, a
/// still is stood in at the picture's own size, and the repeat is answered
/// from the row that fetch left behind.
async fn exercise_withheld(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc {
		server_name: peer,
		media_id: WITHHELD_MEDIA_ID,
	};

	let dim = Dim::new(1024, 768, None);
	let user = fixture_user(services)?;
	let fetched = fetched_once(services, &mxc, &dim, Animate::from(None), &user).await?;

	stood_in_for_the_animation(&fetched, "sentinel")
}

/// A picture the peer only points at is walked by the gate that withholds it.
///
/// Nothing files a redirected object, so the answer carries no walk of its own
/// and a forbidding request has to take one. This is the only exercise that
/// reaches that arm, every other one being answered with the picture itself.
async fn exercise_location(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc {
		server_name: peer,
		media_id: LOCATION_MEDIA_ID,
	};

	let dim = Dim::new(320, 240, None);
	let user = fixture_user(services)?;
	let before = ANSWERED.load(Ordering::Relaxed);

	let fetch = || {
		services
			.media
			.get_or_fetch_thumbnail(&mxc, &dim, Animate::from(None), TIMEOUT, &user)
	};

	let fetched = fetch().await?;

	// one redirected object costs two requests, the location and what it points
	// at, where every other leg answers in one
	if asked(before) != 2 {
		return Err!(
			"the redirected request reached the peer {} times, expected 2",
			asked(before)
		);
	}

	fetch().await?;

	if asked(before) != 2 {
		return Err!("the redirected repeat went back to the peer, {} in total", asked(before));
	}

	stood_in_for_the_animation(&fetched, "redirected")
}

/// A size that is not a bucket is asked for, and cached, at the bucket it
/// takes.
///
/// The dimension a fetch asks the peer for is the dimension it files the answer
/// under, and every later lookup seeks the normalized one, so asking at the
/// size the client named left a row nothing could find and sent every repeat
/// back over federation. Asking at the bucket makes the two agree, and also
/// keeps the row count per media on the ladder rather than one row per distinct
/// size a client happens to name.
async fn exercise_bucket(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc {
		server_name: peer,
		media_id: BUCKET_MEDIA_ID,
	};

	// 100x100 takes the 320x240 bucket, so raw and normalized differ
	let dim = Dim::new(100, 100, None);
	let user = fixture_user(services)?;
	let before = ANSWERED.load(Ordering::Relaxed);

	fetched_once(services, &mxc, &dim, Animate::Allowed, &user).await?;

	// the cache converging says a row was filed where the lookup reads it, not
	// that the peer was asked at the bucket, which is the change under test
	asked_with(320, 240, "scale")?;

	// 150x150 takes the same bucket, so it is the same row
	let sibling = Dim::new(150, 150, None);

	services
		.media
		.get_or_fetch_thumbnail(&mxc, &sibling, Animate::Allowed, TIMEOUT, &user)
		.await?;

	match asked(before) == 1 {
		| true => Ok(()),
		| false =>
			Err!("a sibling size in the same bucket refetched, {} in total", asked(before)),
	}
}

/// The answer is a generated still rather than the picture the peer holds.
///
/// Every forbidding request asserts the same two things, that the animation did
/// not reach the caller and that what did is a thumbnail this server encoded,
/// so the leg names itself and the rest is shared.
fn stood_in_for_the_animation(fetched: &Media, leg: &str) -> Result {
	if fetched.content == GIF {
		return Err!("a forbidding {leg} request was answered with the peer's animation");
	}

	let served = fetched
		.content_type
		.as_deref()
		.unwrap_or_default();

	match served == STILL {
		| true => Ok(()),
		| false => Err!("the {leg} stand-in was served as {served}, expected {STILL}"),
	}
}
