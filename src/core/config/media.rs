//! Configures media storage and processing.
//!
//! Upload and storage settings control how originals enter the repository.
//! Thumbnail, URL preview, and import settings govern derived and migrated media.

use std::{collections::BTreeSet, net::IpAddr, path::PathBuf};

use either::Either;
use regex::RegexSet;
use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::{deserialize_bytesize_u64, deserialize_bytesize_usize, true_fn};

/// Configures media uploads, storage, and access.
///
/// Provider selection controls where media is read and written. Import,
/// thumbnail, and URL preview settings govern related processing.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.media",
	group = "true",
	ignore = "import thumbnail url_preview video_thumbnail"
)]
pub struct Media {
	/// Limits the number of concurrent pending asynchronous media uploads per
	/// user.
	///
	/// reloadable: yes
	/// default: 5
	/// legacy: max_pending_media_uploads
	#[serde(default = "default_max_pending_uploads")]
	pub max_pending_uploads: usize,

	/// Sets the lifetime in seconds before an unused pending MXC URI expires
	/// and is removed.
	///
	/// reloadable: yes
	/// default: 86400 (24 hours)
	/// legacy: media_create_unused_expiration_time
	#[serde(default = "default_create_unused_expiration_time")]
	pub create_unused_expiration_time: u64,

	/// Limits media create requests per second from one user.
	///
	/// reloadable: yes
	/// default: 10
	/// legacy: media_rc_create_per_second
	#[serde(default = "default_rc_create_per_second")]
	pub rc_create_per_second: u32,

	/// Limits the burst count of media create requests from one user.
	///
	/// reloadable: yes
	/// default: 50
	/// legacy: media_rc_create_burst_count
	#[serde(default = "default_rc_create_burst_count")]
	pub rc_create_burst_count: u32,

	/// Sets the writable space threshold below which local media uploads are
	/// refused.
	///
	/// Each local storage provider receiving the upload is checked, excluding
	/// blocks reserved for privileged processes. Media is stored beside the
	/// database by default, and a full disk prevents database writes, so
	/// uploads stop first with M_RESOURCE_LIMIT_EXCEEDED and resume when space
	/// returns. Remote media, URL previews, and thumbnails are never refused.
	/// Accepts an integer byte count or an SI/IEC string such as "10 GiB"; 0
	/// disables the check for cases such as a network mount reporting no free
	/// space.
	///
	/// reloadable: yes
	/// default: "1 GiB"
	/// legacy: media_upload_min_free_space
	#[serde(
		default = "default_upload_min_free_space",
		deserialize_with = "deserialize_bytesize_u64"
	)]
	pub upload_min_free_space: u64,

	/// Prevents media from being embedded in frames, including same-origin
	/// frames.
	///
	/// The default is disabled for compatibility with attachment viewers.
	/// Enabling it adds `frame-ancestors 'none'` to download and thumbnail
	/// Content-Security-Policy headers. Changes require a restart, and reverse
	/// proxy headers may still prevent framing.
	///
	/// legacy: media_deny_framing
	#[serde(default)]
	pub deny_framing: bool,

	/// Prevents inline styles in media documents.
	///
	/// The default is disabled so browser image viewers retain their
	/// presentation styles. Enabling it omits `style-src 'unsafe-inline'` from
	/// download and thumbnail Content-Security-Policy headers. Changes require
	/// a restart, and reverse proxy headers may still restrict styles.
	///
	/// legacy: media_deny_inline_styles
	#[serde(default)]
	pub deny_inline_styles: bool,

	/// Enables the legacy unauthenticated Matrix media repository endpoints.
	///
	/// These are `/_matrix/media/*/config`, `/_matrix/media/*/preview_url`,
	/// `/_matrix/media/*/download/*`, and `/_matrix/media/*/thumbnail/*`. The
	/// authenticated equivalents are always enabled. The default is false.
	///
	/// legacy: allow_legacy_media
	#[serde(default)]
	pub allow_legacy: bool,

	/// Falls back to requesting legacy unauthenticated media from remote
	/// servers.
	///
	/// Unauthenticated media was removed around the third quarter of 2024.
	/// Enabling this adds considerable federation requests that are unlikely
	/// to succeed.
	///
	/// reloadable: yes
	/// legacy: request_legacy_media
	#[serde(default)]
	pub request_legacy: bool,

	/// Permits fetching remote legacy content and thumbnails when true.
	///
	/// When false, those remote legacy requests are rejected.
	///
	/// reloadable: yes
	/// legacy: freeze_legacy_media
	#[serde(default = "true_fn")]
	pub freeze_legacy: bool,

	/// Checks media directory consistency at startup.
	///
	/// Together with `media.compat_file_link`, this upgrades media when
	/// switching between Conduit and tuwunel; both options must be enabled.
	/// Startup checks can also remove database entries for files deleted from
	/// the media directory. If neither check applies and the directory is
	/// large, disabling this may reduce startup time.
	///
	/// legacy: media_startup_check
	#[serde(default = "true_fn")]
	pub startup_check: bool,

	/// Creates media symlinks for compatibility with Conduit's media
	/// directory.
	///
	/// This is needed only when planning to use Conduit again; disabling it
	/// reduces filesystem clutter and symlink management overhead. It is
	/// disabled by default. Before returning to upstream Conduit, run tuwunel
	/// at least once with this enabled and allow `media.startup_check` to
	/// finish before shutdown.
	///
	/// legacy: media_compat_file_link
	#[serde(default)]
	pub compat_file_link: bool,

	/// Removes missing media metadata during startup checks.
	///
	/// Deleting files from the media directory then removes their
	/// corresponding database entries. This is disabled by default because
	/// accidentally moving the directory or making it inaccessible would
	/// otherwise lose database metadata.
	///
	/// legacy: prune_missing_media
	#[serde(default)]
	pub prune_missing: bool,

	/// Selects storage providers used for media.
	///
	/// Configure each listed name at `global.storage_provider.<NAME>.<brand>`;
	/// multiple entries support advanced features and future extensions, but
	/// require checking related settings to avoid resource duplication costs
	/// and poor performance. The default `["media"]` is an implicit provider
	/// for the local media directory. Configure
	/// `global.storage_provider.media.local` to alter it, or omit "media" from
	/// this list to disable it. Existing deployments should retain "media" as
	/// a fallback alongside a new provider.
	///
	/// reloadable: yes
	/// default: ["media"]
	/// legacy: media_storage_providers
	#[allow(
		clippy::doc_link_with_quotes,
		clippy::allow_attributes,
		reason = "configuration arrays contain literal quoted strings, not intra-doc links"
	)]
	#[serde(default = "default_storage_providers")]
	pub storage_providers: BTreeSet<String>,

	/// Selects configured storage providers that receive new media.
	///
	/// When not explicitly configured, all entries in
	/// `media.storage_providers` are used. Each selected provider must also
	/// appear there. This supports passive migration by writing to a new
	/// provider while querying the old one as a fallback, for example
	/// `media.storage_providers = ["media", "media_on_s3"]` with
	/// `media.store_on_providers = ["media_on_s3"]`.
	///
	/// reloadable: yes
	/// default: []
	/// legacy: store_media_on_providers
	#[serde(default)]
	pub store_on_providers: BTreeSet<String>,

	/// Redirects local media downloads when the client requests
	/// `allow_redirect=true` under MSC3860.
	///
	/// If a configured provider can presign the object, such as S3, the server
	/// returns a 307 redirect to a short-lived URL instead of proxying the
	/// bytes. Media stored only on the local filesystem is always served
	/// directly.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: media_allow_redirect
	#[serde(default)]
	pub allow_redirect: bool,

	/// Lists server-name regex patterns from which remote media downloads are
	/// refused.
	///
	/// reloadable: yes
	/// example: ["badserver\.tld$", "badphrase", "19dollarfortnitecards"]
	///
	/// default: []
	/// legacy: prevent_media_downloads_from
	#[serde(default, with = "serde_regex")]
	pub prevent_downloads_from: RegexSet,

	/// Settings for media import.
	///
	/// display: section
	#[serde(default)]
	pub import: Import,

	/// Settings for media thumbnail.
	///
	/// display: section
	#[serde(default)]
	pub thumbnail: Thumbnail,

	/// Settings for media url preview.
	///
	/// display: section
	#[serde(default)]
	pub url_preview: UrlPreview,

	/// Settings for media video thumbnail.
	///
	/// display: section
	#[serde(default)]
	pub video_thumbnail: VideoThumbnail,
}

/// Configures media imported from other server implementations.
///
/// Source settings describe the existing media layout. Conduit imports can
/// read filesystem or object-store originals.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.media.import",
	group = "true",
	ignore = "conduit"
)]
pub struct Import {
	/// Settings for media import conduit.
	///
	/// display: section
	#[serde(default)]
	pub conduit: ImportConduit,
}

/// Describes source media for a Conduit database import.
///
/// Directory settings identify the source sharding layout. An optional
/// storage provider supplies originals from Conduit object storage.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.media.import.conduit",
	group = "true"
)]
pub struct ImportConduit {
	/// Selects Conduit's media directory when importing its database in place.
	///
	/// Leave unset to use `<database_path>/media`, which is Conduit's default
	/// location.
	///
	/// example: "/var/lib/matrix-conduit/media"
	/// legacy: conduit_source_media_path
	pub source_path: Option<PathBuf>,

	/// Sets the source media directory sharding depth for a Conduit import.
	///
	/// Use 0 for a flat directory. This must match the source Conduit's
	/// `media.directory_structure`; the default matches its `Deep { length =
	/// 2, depth = 2 }`.
	///
	/// default: 2
	/// legacy: conduit_media_directory_depth
	#[serde(default = "default_directory_depth")]
	pub directory_depth: u8,

	/// Sets the source shard-segment length for a Conduit media import.
	///
	/// Use it with `media.import.conduit.directory_depth` to describe the
	/// source directory layout.
	///
	/// default: 2
	/// legacy: conduit_media_directory_length
	#[serde(default = "default_directory_length")]
	pub directory_length: u8,

	/// Selects the storage provider containing source originals for a Conduit
	/// S3 import.
	///
	/// Name a `[global.storage_provider.<name>]` entry, or leave unset to read
	/// the filesystem at `media.import.conduit.source_path`. Configure that
	/// provider with Conduit's S3 credentials and set `base_path` to Conduit's
	/// `media.path` prefix. Content-addressed object keys use
	/// `media.import.conduit.directory_depth` and
	/// `media.import.conduit.directory_length` for sharding. Restrict
	/// `media.storage_providers` to destination providers, such as
	/// `["media"]`, or the importer also copies media back into the read-only
	/// source bucket.
	///
	/// example: "conduit_source"
	/// legacy: conduit_source_media_provider
	pub source_provider: Option<String>,
}

/// Configures image and animated thumbnail generation.
///
/// Pixel and frame limits bound decoding and encoding work. Animated job
/// admission limits source processing concurrency.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.media.thumbnail",
	group = "true"
)]
pub struct Thumbnail {
	/// Limits the pixel count the thumbnailer will decode.
	///
	/// Images declaring more pixels receive no thumbnail because dimensions
	/// consume memory regardless of encoded file size; video frames inherit
	/// the video resolution and share this bound. The default 50 megapixels is
	/// roughly four 8K frames and exceeds ordinary camera output. Four bytes
	/// are budgeted per pixel, admitting about 200 MiB per decode by default.
	/// Size this budget against concurrent requests rather than one decode,
	/// because thumbnail requests are not otherwise limited in number.
	///
	/// reloadable: yes
	/// default: 50000000
	/// legacy: media_thumbnail_max_pixels
	#[serde(default = "default_max_pixels")]
	pub max_pixels: u64,

	/// Serves animated thumbnails when requested under
	/// [MSC2705](https://github.com/matrix-org/matrix-spec-proposals/pull/2705).
	///
	/// A requesting client receives an animated GIF when the source has a
	/// frame sequence and a still otherwise; clients that do not request
	/// animation always receive a still. Encoding quantizes each frame with
	/// its own palette and costs much more than a still, so frame count and
	/// total pixel limits also bound the work. Disabling this produces still
	/// PNG thumbnails only.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: media_thumbnail_animated
	#[serde(default = "true_fn")]
	pub animated: bool,

	/// Limits the number of frames in an animated thumbnail.
	///
	/// Longer sources are truncated into a shorter loop without notifying the
	/// client, instead of being refused a thumbnail. Frames also share the
	/// total `media.thumbnail.max_pixels` budget, so animation stops at
	/// whichever limit is reached first. Both limits bound work that any
	/// remote server can request.
	///
	/// reloadable: yes
	/// default: 50
	/// legacy: media_thumbnail_max_frames
	#[serde(default = "default_max_frames")]
	pub max_frames: usize,

	/// Limits source jobs admitted for animated thumbnail generation.
	///
	/// A job acquires a slot before reading the full source and holds it
	/// through source I/O, animated storage, and subsequent still or video
	/// work until generation returns. A running blocking worker retains the
	/// slot after request cancellation. Static images and videos also require
	/// admission because their bytes determine whether animation work is
	/// needed. Changes require a restart.
	///
	/// default: 4
	/// legacy: media_thumbnail_animated_concurrency
	#[serde(default = "default_animated_concurrency")]
	pub animated_concurrency: usize,
}

/// Configures URL preview fetching, caching, and origin access.
///
/// Allowlists and denylists select permitted requests. Size limits, request
/// headers, and cache lifetime control generated previews.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.media.url_preview",
	group = "true"
)]
pub struct UrlPreview {
	/// Selects an optional source IP address or interface for URL preview
	/// requests.
	///
	/// When unset, requests are not bound to a specific address or interface.
	/// Interface names are supported only on Linux, Android, and Fuchsia;
	/// other platforms can specify an IP address. Use `ip link show` to list
	/// system interfaces.
	///
	/// example: `"eth0"` or `"1.2.3.4"`
	/// default:
	/// legacy: url_preview_bound_interface
	#[serde(default, with = "either::serde_untagged_optional")]
	pub bound_interface: Option<Either<IpAddr, String>>,

	/// Allows URL preview requests to domains containing a listed string.
	///
	/// Matching is by containment rather than equality: "google.com" matches
	/// both "https://google.com" and
	/// "http://mymaliciousdomainexamplegoogle.com". A value of "*" allows all
	/// URL previews. This exposes significant server attack surface and
	/// requires understanding the risks.
	///
	/// reloadable: yes
	/// default: []
	/// legacy: url_preview_domain_contains_allowlist
	#[serde(default)]
	pub domain_contains_allowlist: Vec<String>,

	/// Allows URL preview requests to explicitly listed domains.
	///
	/// Matching is exact rather than by containment: "google.com" matches
	/// "https://google.com" and "http://google.com", but not
	/// "https://mymaliciousdomainexamplegoogle.com". A value of "*" allows all
	/// URL previews. This exposes significant server attack surface and
	/// requires understanding the risks.
	///
	/// reloadable: yes
	/// default: []
	/// legacy: url_preview_domain_explicit_allowlist
	#[serde(default)]
	pub domain_explicit_allowlist: Vec<String>,

	/// Rejects URL preview requests to explicitly listed domains.
	///
	/// Matching is exact rather than by containment: "google.com" matches
	/// "https://google.com" and "http://google.com", but not
	/// "https://mymaliciousdomainexamplegoogle.com". The denylist is checked
	/// before allowlists. A value of "*" has no effect.
	///
	/// reloadable: yes
	/// default: []
	/// legacy: url_preview_domain_explicit_denylist
	#[serde(default)]
	pub domain_explicit_denylist: Vec<String>,

	/// Allows URL preview requests to URLs containing a listed string.
	///
	/// Matching is by containment rather than equality: "google.com" matches
	/// "https://google.com/",
	/// "https://google.com/url?q=https://mymaliciousdomainexample.com", and
	/// "https://mymaliciousdomainexample.com/hi/google.com". A value of "*"
	/// allows all URL previews. This exposes significant server attack surface
	/// and requires understanding the risks.
	///
	/// reloadable: yes
	/// default: []
	/// legacy: url_preview_url_contains_allowlist
	#[serde(default)]
	pub url_contains_allowlist: Vec<String>,

	/// Limits the body size read when fetching a URL preview page.
	///
	/// Accepts an integer byte count or an SI/IEC string such as "768 KiB". A
	/// page whose OpenGraph tags fall beyond the limit produces an empty
	/// preview, so pages beginning with large script blocks need a larger
	/// budget.
	///
	/// reloadable: yes
	/// default: 786432
	/// legacy: url_preview_max_spider_size
	#[serde(
		default = "default_max_spider_size",
		deserialize_with = "deserialize_bytesize_usize"
	)]
	pub max_spider_size: usize,

	/// Limits each media item fetched or relayed for a URL preview.
	///
	/// This covers the og:image measurement fetch and lazy-media relay. Media
	/// with an advertised length above the limit is not registered, and a
	/// relay that would exceed it is refused. Accepts an integer byte count or
	/// an SI/IEC string such as "50 MiB".
	///
	/// reloadable: yes
	/// default: "50 MiB"
	/// legacy: url_preview_max_media_size
	#[serde(
		default = "default_preview_max_size",
		deserialize_with = "deserialize_bytesize_usize"
	)]
	pub max_media_size: usize,

	/// Sets the lifetime in seconds of cached URL previews, including empty
	/// previews.
	///
	/// Increasing this reduces requests to origins when room history is read
	/// often, at the cost of serving OpenGraph metadata that may have changed.
	/// A value of 0 fetches on every request. Raising it beyond seven days
	/// takes effect at the next restart because the database applies its
	/// retention floor when opened.
	///
	/// reloadable: yes
	/// default: 86400
	/// legacy: url_preview_cache_ttl
	#[serde(default = "default_cache_ttl")]
	pub cache_ttl: u64,

	/// Applies domain allowlist checks to the root domain when enabled.
	///
	/// This affects both contains and explicit domain allowlists, but not the
	/// URL contains allowlist, and defaults to false. Allowing "wikipedia.org"
	/// then permits subdomains such as "en.m.wikipedia.org" because the root
	/// domain matches. Use this when domain containment is too broad but all
	/// subdomains of a root should be allowed.
	///
	/// reloadable: yes
	/// legacy: url_preview_check_root_domain
	#[serde(default)]
	pub check_root_domain: bool,

	/// Sets the User-Agent used to fetch preview pages and extract OpenGraph
	/// tags.
	///
	/// When unset, the versioned server User-Agent is followed by "preview",
	/// such as "Tuwunel/1.8.1 preview". Some origins serve OpenGraph tags only
	/// to recognized preview crawlers and otherwise place them beyond
	/// `media.url_preview.max_spider_size`.
	///
	/// reloadable: yes
	/// default:
	/// legacy: url_preview_user_agent
	#[serde(default)]
	pub user_agent: Option<String>,

	/// Sets the User-Agent used to fetch and relay URL preview media files.
	///
	/// This covers og:image, og:video, og:audio, and direct links rather than
	/// their containing pages. When unset, it falls back to
	/// `media.url_preview.user_agent`, then the versioned server User-Agent.
	///
	/// reloadable: yes
	/// default:
	/// legacy: url_preview_media_user_agent
	#[serde(default)]
	pub media_user_agent: Option<String>,

	/// Sets Accept-Language for URL preview page and media requests.
	///
	/// For example, "en-US,en;q=0.9" requests English from sites supporting
	/// language negotiation, while sites choosing a language solely by IP may
	/// ignore it. When unset, no Accept-Language header is sent. Changes
	/// affect new requests; cached previews retain their language until
	/// expiry.
	///
	/// reloadable: yes
	/// default:
	/// legacy: url_preview_accept_language
	#[serde(default)]
	pub accept_language: Option<String>,
}

/// Configures external video frame extraction for thumbnails.
///
/// The command writes a still image for ordinary thumbnail processing.
/// Time, size, and concurrency limits bound extraction and staging work.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.media.video_thumbnail",
	group = "true"
)]
pub struct VideoThumbnail {
	/// Selects the program used to extract a still frame for videos uploaded
	/// without a thumbnail.
	///
	/// We do not decode video internally; we scale and crop the extracted
	/// frame like any other image, then cache an ordinary thumbnail. The list
	/// is an argument vector with the program first and its arguments after
	/// it, executed directly without a shell. Before each call, every argument
	/// substitutes `{input}` with the temporary source-video path and
	/// `{width}` and `{height}` with the requested dimensions. The program
	/// writes one frame to standard output as PNG, JPEG, WebP, or GIF; an
	/// empty list serves videos without thumbnails.
	///
	/// reloadable: yes
	/// example: [
	/// "ffmpeg", "-loglevel", "error", "-i", "{input}", "-vf", "thumbnail",
	/// "-frames:v", "1", "-f", "image2pipe", "-c:v", "mjpeg", "pipe:1",
	/// ]
	///
	/// default: []
	/// legacy: media_video_thumbnail_command
	#[serde(default)]
	pub command: Vec<String>,

	/// Limits the seconds a video thumbnail request can spend extracting a
	/// frame.
	///
	/// One deadline covers waiting for a slot, staging the video, and running
	/// the program, preventing queueing from multiplying the timeout. Expiry
	/// kills the program and anything it spawned, then serves the video
	/// without a thumbnail. Extraction requires a nonempty
	/// `media.video_thumbnail.command`.
	///
	/// reloadable: yes
	/// default: 30
	/// legacy: media_video_thumbnail_timeout
	#[serde(default = "default_timeout")]
	pub timeout: u64,

	/// Limits simultaneous video thumbnail extractions.
	///
	/// Video decoding costs much more than image scaling, so excess requests
	/// wait for a slot instead of adding host load. Slots span video staging
	/// through program exit, also limiting the number of staged videos.
	/// Increase this when cores are available; changes require a restart. This
	/// applies only when `media.video_thumbnail.command` is nonempty.
	///
	/// default: 1
	/// legacy: media_video_thumbnail_concurrency
	#[serde(default = "default_concurrency")]
	pub concurrency: usize,

	/// Limits the byte size of staged videos and extracted frames.
	///
	/// Oversized videos are served without thumbnails rather than staged, and
	/// oversized frames are refused rather than decoded from truncated data.
	/// Accepts an integer byte count or an SI/IEC string such as "128 MiB".
	/// Extraction requires a nonempty `media.video_thumbnail.command`.
	///
	/// reloadable: yes
	/// default: "128 MiB"
	/// legacy: media_video_thumbnail_max_size
	#[serde(
		default = "default_max_size",
		deserialize_with = "deserialize_bytesize_usize"
	)]
	pub max_size: usize,

	/// Selects the directory for staging videos read by thumbnail programs.
	///
	/// Each running program has one file, removed when it exits. When unset, a
	/// `tmp` subdirectory of the database path keeps large videos off the
	/// memory-backed `/tmp` commonly provided by service managers. Files left
	/// by a killed server are reclaimed at startup even when
	/// `media.video_thumbnail.command` is empty. Extraction itself requires a
	/// nonempty command.
	///
	/// reloadable: yes
	/// example: "/var/tmp/tuwunel"
	/// legacy: media_video_thumbnail_path
	pub path: Option<PathBuf>,
}

impl Default for Media {
	fn default() -> Self {
		Self {
			max_pending_uploads: default_max_pending_uploads(),
			create_unused_expiration_time: default_create_unused_expiration_time(),
			rc_create_per_second: default_rc_create_per_second(),
			rc_create_burst_count: default_rc_create_burst_count(),
			upload_min_free_space: default_upload_min_free_space(),
			deny_framing: Default::default(),
			deny_inline_styles: Default::default(),
			allow_legacy: Default::default(),
			request_legacy: Default::default(),
			freeze_legacy: true_fn(),
			startup_check: true_fn(),
			compat_file_link: Default::default(),
			prune_missing: Default::default(),
			storage_providers: default_storage_providers(),
			store_on_providers: Default::default(),
			allow_redirect: Default::default(),
			prevent_downloads_from: Default::default(),
			import: Import::default(),
			thumbnail: Thumbnail::default(),
			url_preview: UrlPreview::default(),
			video_thumbnail: VideoThumbnail::default(),
		}
	}
}

impl Default for ImportConduit {
	fn default() -> Self {
		Self {
			source_path: None,
			directory_depth: default_directory_depth(),
			directory_length: default_directory_length(),
			source_provider: None,
		}
	}
}

impl Default for Thumbnail {
	fn default() -> Self {
		Self {
			max_pixels: default_max_pixels(),
			animated: true_fn(),
			max_frames: default_max_frames(),
			animated_concurrency: default_animated_concurrency(),
		}
	}
}

impl Default for UrlPreview {
	fn default() -> Self {
		Self {
			bound_interface: Default::default(),
			domain_contains_allowlist: Default::default(),
			domain_explicit_allowlist: Default::default(),
			domain_explicit_denylist: Default::default(),
			url_contains_allowlist: Default::default(),
			max_spider_size: default_max_spider_size(),
			max_media_size: default_preview_max_size(),
			cache_ttl: default_cache_ttl(),
			check_root_domain: Default::default(),
			user_agent: Default::default(),
			media_user_agent: Default::default(),
			accept_language: Default::default(),
		}
	}
}

impl Default for VideoThumbnail {
	fn default() -> Self {
		Self {
			command: Default::default(),
			timeout: default_timeout(),
			concurrency: default_concurrency(),
			max_size: default_max_size(),
			path: None,
		}
	}
}

fn default_max_pending_uploads() -> usize { 5 }

fn default_create_unused_expiration_time() -> u64 { 86400 }

fn default_rc_create_per_second() -> u32 { 10 }

fn default_rc_create_burst_count() -> u32 { 50 }

fn default_upload_min_free_space() -> u64 { 1024 * 1024 * 1024 }

fn default_storage_providers() -> BTreeSet<String> { ["media".to_owned()].into() }

fn default_directory_depth() -> u8 { 2 }

fn default_directory_length() -> u8 { 2 }

fn default_max_pixels() -> u64 { 50_000_000 }

fn default_max_frames() -> usize { 50 }

fn default_animated_concurrency() -> usize { 4 }

fn default_max_spider_size() -> usize {
	768 * 1024 // 768 KiB
}

fn default_preview_max_size() -> usize {
	50 * 1024 * 1024 // 50 MiB
}

fn default_cache_ttl() -> u64 { 60 * 60 * 24 }

fn default_timeout() -> u64 { 30 }

fn default_concurrency() -> usize { 1 }

fn default_max_size() -> usize { 128 * 1024 * 1024 }
