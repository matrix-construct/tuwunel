//! URL Previews
//!
//! This functionality is gated by 'url_preview', but not at the unit level for
//! historical and simplicity reasons. Instead the feature gates the inclusion
//! of dependencies and nulls out results through the existing interface when
//! not featured.

use std::{
	net::IpAddr,
	time::{Duration, SystemTime},
};

#[cfg(feature = "url_preview")]
use reqwest::header::CONTENT_DISPOSITION;
use reqwest::header::{CONTENT_TYPE, COOKIE, HeaderValue, USER_AGENT};
#[cfg(feature = "url_preview")]
use ruma::Mxc;
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Config, Err, Result, debug, err, implement, smallstr::SmallString,
	utils::time::timepoint_from_now,
};
#[cfg(feature = "url_preview")]
use tuwunel_core::{debug_warn, utils::random_string};
#[cfg(feature = "url_preview")]
use tuwunel_database::Txn;
use url::{Host, Url};
#[cfg(feature = "url_preview")]
use webpage::OpengraphObject;

#[cfg(feature = "url_preview")]
use super::MXC_LENGTH;
use super::Service;
#[cfg(feature = "url_preview")]
use crate::client::read_response_capped;

/// A media type as declared by a page, inline for every common spelling.
type MediaType = SmallString<[u8; 32]>;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct UrlPreviewData {
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:title"
	)]
	pub title: Option<String>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:description"
	)]
	pub description: Option<String>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:image"
	)]
	pub image: Option<String>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "matrix:image:size"
	)]
	pub image_size: Option<usize>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:image:width"
	)]
	pub image_width: Option<u32>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:image:height"
	)]
	pub image_height: Option<u32>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:video"
	)]
	pub video: Option<String>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:video:type"
	)]
	pub video_type: Option<MediaType>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "matrix:video:size"
	)]
	pub video_size: Option<usize>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:video:width"
	)]
	pub video_width: Option<u32>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:video:height"
	)]
	pub video_height: Option<u32>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:audio"
	)]
	pub audio: Option<String>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "matrix:audio:size"
	)]
	pub audio_size: Option<usize>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:type"
	)]
	pub og_type: Option<String>,
	#[serde(
		default,
		skip_serializing_if = "Option::is_none",
		rename = "og:url"
	)]
	pub og_url: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct CachedPreview {
	pub(super) preview: UrlPreviewData,
	pub(super) expire: SystemTime,
}

impl CachedPreview {
	fn new(ttl: Duration, preview: UrlPreviewData) -> Result<Self> {
		timepoint_from_now(ttl).map(|expire| Self { preview, expire })
	}

	#[inline]
	#[must_use]
	pub(super) fn valid(&self) -> bool { self.expire > SystemTime::now() }
}

/// Which configured agent a preview request speaks as.
///
/// Origins commonly gate a page and the media it references differently, so
/// the two are configured separately.
#[derive(Clone, Copy)]
pub(super) enum Agent {
	Page,
	Media,
}

/// Hosts whose pages carry their `<head>` metadata only for an allowlisted
/// crawler, and which answer oEmbed for any agent.
const YOUTUBE_HOSTS: [&str; 5] = [
	"youtu.be",
	"youtube.com",
	"www.youtube.com",
	"m.youtube.com",
	"music.youtube.com",
];

/// Consent state that suppresses the interstitial Google serves in place of
/// the page in some regions.
///
/// `SOCS` is the cookie Google reads today and `CONSENT` the one older
/// endpoints still honor.
const YOUTUBE_CONSENT_COOKIE: &str = "SOCS=CAI; CONSENT=PENDING+999";

/// Endpoint answering oEmbed for every host in `YOUTUBE_HOSTS`, including
/// the short and subdomain forms.
#[cfg(feature = "url_preview")]
const YOUTUBE_OEMBED: &str = "https://www.youtube.com/oembed";

/// An oEmbed document runs to a few hundred bytes; the cap bounds only a
/// hostile origin.
#[cfg(feature = "url_preview")]
const OEMBED_MAX_SIZE: usize = 64 * 1024;

/// The oEmbed fields a preview can carry.
///
/// Every other field of the document is ignored, and each of these is
/// optional in the specification.
#[cfg(feature = "url_preview")]
#[derive(Deserialize)]
struct Oembed {
	#[serde(rename = "type")]
	kind: Option<String>,
	title: Option<String>,
	author_name: Option<String>,
	thumbnail_url: Option<String>,
}

#[implement(Service)]
pub async fn get_url_preview(&self, url: &Url) -> Result<UrlPreviewData> {
	if let Ok(cached) = self.db.get_url_preview(url.as_str()).await {
		return Ok(cached.preview);
	}

	// ensure that only one request is made per URL
	let _request_lock = self.url_preview_mutex.lock(url.as_str()).await;

	match self.db.get_url_preview(url.as_str()).await {
		| Ok(cached) => Ok(cached.preview),
		| Err(_) => self.request_url_preview(url).await,
	}
}

#[implement(Service)]
pub async fn request_url_preview(&self, url: &Url) -> Result<UrlPreviewData> {
	self.check_url_host(url)?;

	let response = self.preview_get(url, Agent::Page).send().await?;

	debug!(?url, "URL preview response headers: {:?}", response.headers());

	self.check_remote_addr(&response)?;

	// an upstream error response must not be turned into a cached preview.
	// origins commonly gate pages and media differently by agent, so when a
	// distinct media agent is configured, a page-agent rejection is not
	// final: the URL may be a direct media link acceptable to the media
	// client (see media_refetch for the successful counterpart).
	let status = response.status();
	let (response, via_media_client) = if status.is_success() {
		(response, false)
	} else if self
		.services
		.config
		.url_preview_media_user_agent
		.is_some()
	{
		(self.media_response(url).await?, true)
	} else {
		return Err!(Request(NotFound(debug_warn!(
			?status,
			%url,
			"URL preview request failed"
		))));
	};

	let content_type = response
		.headers()
		.get(CONTENT_TYPE)
		.ok_or_else(|| err!(Request(Unknown("Missing Content-Type header"))))?
		.to_str()
		.map_err(|e| err!(Request(Unknown("Invalid Content-Type header: {e}"))))?
		.to_owned();

	let data = match content_type.as_str() {
		| html if html.starts_with("text/html") => {
			// pages are only crawled with the page client; its rejection
			// stands even when the media client was served a page
			if via_media_client {
				return Err!(Request(NotFound(debug_warn!(
					?status,
					%url,
					"URL preview request failed"
				))));
			}

			let data = self.download_html(url, response).await?;

			self.oembed_recover(url, data).await
		},
		| img if img.starts_with("image/") => {
			let response = self
				.media_refetch(url, response, via_media_client)
				.await?;

			require_media_type(&response, "image/")?;
			self.download_image(response).await?
		},
		| video if video.starts_with("video/") => {
			let response = self
				.media_refetch(url, response, via_media_client)
				.await?;

			require_media_type(&response, "video/")?;
			self.download_video(response).await?
		},
		| audio if audio.starts_with("audio/") => {
			let response = self
				.media_refetch(url, response, via_media_client)
				.await?;

			require_media_type(&response, "audio/")?;
			self.download_audio(response).await?
		},
		| _ => return Err!(Request(Unknown("Unsupported Content-Type"))),
	};

	let ttl = Duration::from_secs(self.services.config.url_preview_cache_ttl);
	let cached = CachedPreview::new(ttl, data)?;

	self.db.set_url_preview(url.as_str(), &cached)?;

	Ok(cached.preview)
}

/// Build a preview request through the preview client, carrying the headers
/// `preview_headers` applies.
#[implement(Service)]
fn preview_get(&self, url: &Url, agent: Agent) -> reqwest::RequestBuilder {
	let request = self.services.client.url_preview.get(url.as_str());

	self.preview_headers(request, url, agent)
}

/// Apply the configured User-Agent and any origin-specific headers to a
/// preview request.
///
/// Both are read per request rather than baked into the client, so a
/// configuration reload takes effect without restarting the server. The
/// configuration is bound once so the two agent options are read through a
/// single handle.
#[implement(Service)]
pub(super) fn preview_headers(
	&self,
	request: reqwest::RequestBuilder,
	url: &Url,
	agent: Agent,
) -> reqwest::RequestBuilder {
	let config: &Config = &self.services.config;
	let user_agent = match agent {
		| Agent::Page => config.url_preview_user_agent.as_deref(),
		| Agent::Media => config
			.url_preview_media_user_agent
			.as_deref()
			.or(config.url_preview_user_agent.as_deref()),
	};

	let request = match user_agent {
		| Some(user_agent) => request.header(USER_AGENT, user_agent),
		| None => request,
	};

	// the consent cookie is scoped to the hosts that gate on it so it never
	// travels to another origin
	match is_youtube(url) {
		| true => request.header(COOKIE, HeaderValue::from_static(YOUTUBE_CONSENT_COOKIE)),
		| false => request,
	}
}

#[must_use]
fn is_youtube(url: &Url) -> bool {
	url.host_str()
		.is_some_and(|host| YOUTUBE_HOSTS.contains(&host))
}

/// Screen a preview response's peer address against the CIDR denylist.
///
/// A missing peer address cannot be screened, so it fails closed.
#[implement(Service)]
fn check_remote_addr(&self, response: &reqwest::Response) -> Result {
	let Some(remote_addr) = response.remote_addr() else {
		return Err!(Request(Forbidden("URL preview response has no peer address")));
	};

	debug!(url = %response.url(), ?remote_addr, "URL preview response remote address");

	self.services
		.client
		.valid_cidr_range_remote_addr(response.url(), remote_addr)
		.then_some(())
		.ok_or_else(|| err!(Request(Forbidden("Requesting from this address is forbidden"))))
}

/// Recover a preview from the origin's oEmbed endpoint when the page yielded
/// nothing usable.
///
/// Some origins serve their `<head>` metadata only to an agent they
/// recognise as a link-preview crawler, while answering oEmbed for anyone.
/// A page that parsed to nothing is therefore worth one much smaller second
/// request, and the original preview stands if that request fails too.
#[cfg(feature = "url_preview")]
#[implement(Service)]
async fn oembed_recover(&self, url: &Url, data: UrlPreviewData) -> UrlPreviewData {
	// an already-staged image would be orphaned by replacing the preview
	if data.title.is_some() || data.image.is_some() {
		return data;
	}

	let Some(endpoint) = oembed_endpoint(url) else {
		return data;
	};

	self.oembed_preview(&endpoint, url)
		.await
		.inspect_err(|e| debug!(%url, %e, "oEmbed recovery failed"))
		.unwrap_or(data)
}

#[cfg(not(feature = "url_preview"))]
#[implement(Service)]
#[expect(clippy::unused_async)]
async fn oembed_recover(&self, _url: &Url, data: UrlPreviewData) -> UrlPreviewData { data }

/// The oEmbed endpoint answering for `url`, when its origin has one.
#[cfg(feature = "url_preview")]
fn oembed_endpoint(url: &Url) -> Option<Url> {
	is_youtube(url)
		.then(|| {
			Url::parse_with_params(YOUTUBE_OEMBED, [("url", url.as_str()), ("format", "json")])
		})
		.and_then(Result::ok)
}

/// Fetch an oEmbed document and render it as a preview.
///
/// The document names a thumbnail rather than carrying one, so the image is
/// measured and staged through the same path an `og:image` takes.
#[cfg(feature = "url_preview")]
#[implement(Service)]
async fn oembed_preview(&self, endpoint: &Url, page: &Url) -> Result<UrlPreviewData> {
	// this host is chosen here rather than named by the page, so the operator's
	// own allowlist decides whether it may be contacted at all
	if !self.url_preview_allowed(endpoint) {
		return Err!(Request(Forbidden(debug_warn!(
			%endpoint,
			"oEmbed endpoint is not allowed for previewing"
		))));
	}

	self.check_url_host(endpoint)?;

	let response = self
		.preview_get(endpoint, Agent::Page)
		.send()
		.await?;

	self.check_remote_addr(&response)?;

	let status = response.status();

	if !status.is_success() {
		return Err!(Request(NotFound(debug_warn!(
			?status,
			%endpoint,
			"oEmbed request failed"
		))));
	}

	let body = read_response_capped(response, OEMBED_MAX_SIZE).await?;
	let oembed: Oembed = serde_json::from_slice(&body)
		.map_err(|e| err!(Request(Unknown("Invalid oEmbed document: {e}"))))?;

	// oEmbed carries no description; the author is the only other prose the
	// document offers and reads as a byline in every client that shows one
	let image = self
		.oembed_image(oembed.thumbnail_url.as_deref())
		.await;

	Ok(UrlPreviewData {
		title: oembed.title,
		description: oembed.author_name,
		video_type: video_type(oembed.kind.as_deref()).map(Into::into),
		og_type: og_type(oembed.kind.as_deref()),
		og_url: Some(page.as_str().to_owned()),
		..image
	})
}

/// Translate an oEmbed `video` document into the type a preview can carry.
///
/// oEmbed hands back an HTML player rather than a media file, so the player
/// type is all a preview can report. Clients read that type on its own to
/// mark the preview playable at the origin.
#[cfg(feature = "url_preview")]
fn video_type(kind: Option<&str>) -> Option<&'static str> {
	kind.eq(&Some("video")).then_some("text/html")
}

/// Translate an oEmbed `type` into the OpenGraph vocabulary the preview
/// response is defined in.
///
/// oEmbed names its own kinds (`video`, `photo`, `link`, `rich`), none of
/// which is an OpenGraph type; anything without a counterpart takes the
/// OpenGraph default the page path would have produced.
#[cfg(feature = "url_preview")]
fn og_type(kind: Option<&str>) -> Option<String> {
	kind.map(|kind| match kind {
		| "video" => "video.other",
		| _ => "website",
	})
	.map(ToOwned::to_owned)
}

/// Measure an oEmbed thumbnail, yielding an empty preview when it is absent
/// or unusable.
///
/// A thumbnail failure must not cost the textual preview the document has
/// already provided.
#[cfg(feature = "url_preview")]
#[implement(Service)]
async fn oembed_image(&self, thumbnail_url: Option<&str>) -> UrlPreviewData {
	let Some(thumbnail) = thumbnail_url
		.and_then(|thumbnail| Url::parse(thumbnail).ok())
		.filter(|thumbnail| ["http", "https"].contains(&thumbnail.scheme()))
	else {
		return UrlPreviewData::default();
	};

	self.preview_image(&thumbnail)
		.await
		.unwrap_or_default()
}

/// Fetch and measure a preview image, keeping the textual preview when the
/// origin refuses it.
///
/// The measurement is a media fetch: it carries the media agent, or the
/// origin could serve the measurement different content than it serves the
/// relayed mxc.
#[cfg(feature = "url_preview")]
#[implement(Service)]
async fn preview_image(&self, image_url: &Url) -> Result<UrlPreviewData> {
	self.check_url_host(image_url)?;

	let response = self
		.preview_get(image_url, Agent::Media)
		.send()
		.await?;

	self.check_remote_addr(&response)?;

	// a failing preview image must not become a preview mxc the relay is
	// guaranteed to reject; skip it and keep the textual preview
	if !response.status().is_success() {
		debug!(
			%image_url,
			status = ?response.status(),
			"Skipping preview image with unsuccessful response"
		);

		return Ok(UrlPreviewData::default());
	}

	self.download_image(response).await
}

/// Download an image for URL preview metadata.
///
/// When URL previews are enabled, the image is staged for lazy media retrieval;
/// otherwise this returns the feature-disabled error.
#[cfg(feature = "url_preview")]
#[implement(Service)]
pub async fn download_image(&self, response: reqwest::Response) -> Result<UrlPreviewData> {
	use image::ImageReader;

	// the image is fetched once here to measure it; the bytes are staged so the
	// first client download promotes them instead of refetching the origin
	let url = response.url().clone();
	let content_type = response
		.headers()
		.get(CONTENT_TYPE)
		.and_then(|value| value.to_str().ok())
		.map(ToOwned::to_owned);

	let content_disposition = response
		.headers()
		.get(CONTENT_DISPOSITION)
		.and_then(|value| value.to_str().ok())
		.map(ToOwned::to_owned);

	let limit = self.services.config.url_preview_max_media_size;
	let image = read_response_capped(response, limit).await?;

	let cursor = std::io::Cursor::new(&image);
	let (width, height) = match ImageReader::new(cursor).with_guessed_format() {
		| Err(_) => (None, None),
		| Ok(reader) => match reader.into_dimensions() {
			| Err(_) => (None, None),
			| Ok((width, height)) => (Some(width), Some(height)),
		},
	};

	let mut txn = self.services.db.txn();
	let mxc = self.queue_lazy_media(&mut txn, url.as_str());

	self.db.set_lazy_content(
		&mut txn,
		&mxc,
		content_type.as_deref(),
		content_disposition.as_deref(),
		&image,
	);

	txn.execute();

	Ok(UrlPreviewData {
		image: Some(mxc),
		image_size: Some(image.len()),
		image_width: width,
		image_height: height,
		..Default::default()
	})
}

/// Download an image for URL preview metadata.
///
/// When URL previews are enabled, the image is staged for lazy media retrieval;
/// otherwise this returns the feature-disabled error.
#[cfg(not(feature = "url_preview"))]
#[implement(Service)]
#[expect(clippy::unused_async)]
pub async fn download_image(&self, _response: reqwest::Response) -> Result<UrlPreviewData> {
	Err!(FeatureDisabled("url_preview"))
}

/// Fetch a URL with the media client, applying the same address and status
/// screening as the page fetch. Direct preview media is measured and
/// registered from the media client's response so it matches what the relay
/// will serve.
#[cfg(feature = "url_preview")]
#[implement(Service)]
async fn media_response(&self, url: &Url) -> Result<reqwest::Response> {
	let response = self.preview_get(url, Agent::Media).send().await?;

	self.check_remote_addr(&response)?;

	if !response.status().is_success() {
		return Err!(Request(NotFound(debug_warn!(
			status = ?response.status(),
			%url,
			"URL preview media request failed"
		))));
	}

	Ok(response)
}

#[cfg(not(feature = "url_preview"))]
#[implement(Service)]
#[expect(clippy::unused_async)]
async fn media_response(&self, _url: &Url) -> Result<reqwest::Response> {
	Err!(FeatureDisabled("url_preview"))
}

/// Replace a page-client response with the media client's for a direct media
/// URL. When no distinct media agent is configured the two clients are
/// identical and the original response is used as-is, avoiding a second
/// request.
#[implement(Service)]
async fn media_refetch(
	&self,
	url: &Url,
	response: reqwest::Response,
	via_media_client: bool,
) -> Result<reqwest::Response> {
	if via_media_client
		|| self
			.services
			.config
			.url_preview_media_user_agent
			.is_none()
	{
		return Ok(response);
	}

	self.media_response(url).await
}

/// Verify a possibly-refetched preview response still carries the content type
/// class the page response was dispatched on, so a media-client refetch that
/// substitutes a different type is not mis-registered.
fn require_media_type(response: &reqwest::Response, class: &str) -> Result {
	response
		.headers()
		.get(CONTENT_TYPE)
		.and_then(|value| value.to_str().ok())
		.is_some_and(|content_type| content_type.starts_with(class))
		.then_some(())
		.ok_or_else(|| err!(Request(Unknown("Unsupported Content-Type"))))
}

/// Mint a local mxc:// URI that resolves to `url` on first download (see
/// `Service::fetch_lazy_media`), keeping preview generation independent of the
/// underlying file size while routing clients through this server.
#[cfg(feature = "url_preview")]
#[implement(Service)]
fn register_lazy_media(&self, url: &str) -> String {
	let mxc = self.mint_lazy_media();

	self.db.insert_lazy_media(&mxc, url);

	mxc
}

#[cfg(feature = "url_preview")]
#[implement(Service)]
fn queue_lazy_media(&self, txn: &mut Txn, url: &str) -> String {
	let mxc = self.mint_lazy_media();

	self.db.queue_lazy_media(txn, &mxc, url);

	mxc
}

#[cfg(feature = "url_preview")]
#[implement(Service)]
fn mint_lazy_media(&self) -> String {
	Mxc {
		server_name: self.services.globals.server_name(),
		media_id: &random_string(MXC_LENGTH),
	}
	.to_string()
}

#[cfg(feature = "url_preview")]
#[implement(Service)]
#[expect(clippy::unused_async)]
pub async fn download_video(&self, response: reqwest::Response) -> Result<UrlPreviewData> {
	let video_size =
		checked_media_size(&response, self.services.config.url_preview_max_media_size)?;

	Ok(UrlPreviewData {
		video: Some(self.register_lazy_media(response.url().as_str())),
		video_size,
		..Default::default()
	})
}

#[cfg(not(feature = "url_preview"))]
#[implement(Service)]
#[expect(clippy::unused_async)]
pub async fn download_video(&self, _response: reqwest::Response) -> Result<UrlPreviewData> {
	Err!(FeatureDisabled("url_preview"))
}

#[cfg(feature = "url_preview")]
#[implement(Service)]
#[expect(clippy::unused_async)]
pub async fn download_audio(&self, response: reqwest::Response) -> Result<UrlPreviewData> {
	let audio_size =
		checked_media_size(&response, self.services.config.url_preview_max_media_size)?;

	Ok(UrlPreviewData {
		audio: Some(self.register_lazy_media(response.url().as_str())),
		audio_size,
		..Default::default()
	})
}

#[cfg(not(feature = "url_preview"))]
#[implement(Service)]
#[expect(clippy::unused_async)]
pub async fn download_audio(&self, _response: reqwest::Response) -> Result<UrlPreviewData> {
	Err!(FeatureDisabled("url_preview"))
}

/// Parse a direct-file preview's advertised size, refusing one over the cap so
/// we never register an mxc the relay is guaranteed to reject at fetch time.
#[cfg(feature = "url_preview")]
fn checked_media_size(response: &reqwest::Response, limit: usize) -> Result<Option<usize>> {
	let size = response
		.content_length()
		.and_then(|len| usize::try_from(len).ok());

	if size.is_some_and(|size| size > limit) {
		return Err!(Request(TooLarge("Media exceeds url_preview_max_media_size")));
	}

	Ok(size)
}

#[cfg(feature = "url_preview")]
#[implement(Service)]
async fn download_html(&self, url: &Url, response: reqwest::Response) -> Result<UrlPreviewData> {
	use webpage::HTML;

	let limit = self.services.config.url_preview_max_spider_size;
	let (bytes, truncated) = spider_body(response, limit).await?;

	// the parser needs an owned string, so the read buffer becomes one rather
	// than being copied into a second buffer of the same size
	let body = String::from_utf8(bytes)
		.unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());

	let Ok(html) = HTML::from_string(body, Some(url.as_str().to_owned())) else {
		return Err!(Request(Unknown("Failed to parse HTML")));
	};

	// twitter:* card tags mirror og:; some pages emit only the twitter set,
	// or (fixvx) an empty og: value beside the real twitter: one
	let twitter = |key| {
		html.meta
			.get(key)
			.map(String::as_str)
			.filter(|content| !content.is_empty())
	};

	// `webpage` does not resolve relative URLs in `og:` meta tags; resolve
	// against the page URL, then keep only the http(s) ones we can fetch
	let image_url = html
		.opengraph
		.images
		.first()
		.map(|obj| obj.url.as_str())
		.filter(|image| !image.is_empty())
		.or_else(|| twitter("twitter:image"))
		.or_else(|| twitter("twitter:image:src"))
		.map(|image| url.join(image))
		.transpose()
		.map_err(|e| err!(Request(Unknown("Invalid preview image URL: {e}"))))?
		.filter(|image_url| ["http", "https"].contains(&image_url.scheme()));

	let mut data = match image_url {
		| None => UrlPreviewData::default(),
		| Some(image_url) => self.preview_image(&image_url).await?,
	};

	if let Some(obj) = html.opengraph.videos.first()
		&& !obj.url.is_empty()
	{
		// the declared type is reported even when the URL cannot be relayed
		data.video_type = obj
			.properties
			.get("type")
			.map(String::as_str)
			.map(Into::into);

		data.video_width = obj
			.properties
			.get("width")
			.and_then(|w| w.parse().ok());

		data.video_height = obj
			.properties
			.get("height")
			.and_then(|h| h.parse().ok());

		data.video = self.lazy_media(url, obj, "video/");
	}

	if let Some(obj) = html.opengraph.audios.first()
		&& !obj.url.is_empty()
	{
		data.audio = self.lazy_media(url, obj, "audio/");
	}

	let props = html.opengraph.properties;

	data.title = props
		.get("title")
		.cloned()
		.filter(|title| !title.is_empty())
		.or_else(|| twitter("twitter:title").map(ToOwned::to_owned))
		.or(html.title);

	data.description = props
		.get("description")
		.cloned()
		.filter(|description| !description.is_empty())
		.or_else(|| twitter("twitter:description").map(ToOwned::to_owned))
		.or(html.description);

	data.og_type = Some(html.opengraph.og_type);
	data.og_url = props.get("url").cloned();

	// a page whose head metadata sits past the cap parses clean and yields
	// nothing, which is indistinguishable from a page carrying no tags
	if truncated && data.title.is_none() && data.description.is_none() && data.image.is_none() {
		debug_warn!(
			%url,
			%limit,
			"Preview page was truncated before any metadata was found; a larger \
			 url_preview_max_spider_size or a different url_preview_user_agent may be needed"
		);
	}

	Ok(data)
}

#[cfg(not(feature = "url_preview"))]
#[implement(Service)]
#[expect(clippy::unused_async)]
async fn download_html(
	&self,
	_url: &Url,
	_response: reqwest::Response,
) -> Result<UrlPreviewData> {
	Err!(FeatureDisabled("url_preview"))
}

/// Read a page body up to `limit`, reporting whether the cap cut it short.
///
/// An advertised length seeds the buffer, and growth past that stays
/// geometric but never exceeds the cap, so a truncated page costs the cap
/// rather than the next power of two above it.
#[cfg(feature = "url_preview")]
async fn spider_body(mut response: reqwest::Response, limit: usize) -> Result<(Vec<u8>, bool)> {
	let hint = response
		.content_length()
		.and_then(|len| usize::try_from(len).ok())
		.map_or(0, |len| len.min(limit));

	let mut bytes: Vec<u8> = Vec::with_capacity(hint);

	while let Some(chunk) = response.chunk().await? {
		let want = chunk.len().min(limit.saturating_sub(bytes.len()));

		reserve_capped(&mut bytes, want, limit);
		bytes.extend_from_slice(&chunk[..want]);

		if want < chunk.len() {
			return Ok((bytes, true));
		}
	}

	Ok((bytes, false))
}

/// Reserve `want` more bytes, growing geometrically but never past `limit`.
///
/// `want` is expected to be clamped to the remaining budget by the caller; a
/// larger value is honored rather than dropped, since refusing to reserve it
/// would only move the allocation into the following `extend_from_slice`.
#[cfg(feature = "url_preview")]
fn reserve_capped(bytes: &mut Vec<u8>, want: usize, limit: usize) {
	let need = bytes.len().saturating_add(want);

	if need <= bytes.capacity() {
		return;
	}

	let target = bytes
		.capacity()
		.saturating_mul(2)
		.clamp(need, limit.max(need));

	bytes.reserve_exact(target.saturating_sub(bytes.len()));
}

/// Mint an `mxc://` URI for a page's declared media, or nothing when it is not
/// relayable.
///
/// The URL is recorded rather than fetched, so a page naming a large video
/// costs the preview request no bandwidth; it is fetched and checked only once
/// a client asks for the resulting URI. Screening IP literals here as well
/// keeps a preview from handing out a URI that the same check at relay time is
/// guaranteed to refuse.
#[cfg(feature = "url_preview")]
#[implement(Service)]
fn lazy_media(&self, page: &Url, obj: &OpengraphObject, class: &str) -> Option<String> {
	declares_media_type(obj, class)
		.then(|| page.join(&obj.url).ok())
		.flatten()
		.filter(|url| ["http", "https"].contains(&url.scheme()))
		.filter(|url| self.check_url_host(url).is_ok())
		.map(|url| self.register_lazy_media(url.as_str()))
}

/// Whether an OpenGraph media object's declared type belongs to `class`.
///
/// A missing `og:*:type` is accepted, since most origins omit it. A type
/// outside the class means the URL addresses a player page rather than a
/// file, which the relay cannot serve as media.
#[cfg(feature = "url_preview")]
fn declares_media_type(obj: &OpengraphObject, class: &str) -> bool {
	obj.properties
		.get("type")
		.is_none_or(|kind| kind.starts_with(class))
}

#[implement(Service)]
pub(super) fn check_url_host(&self, url: &Url) -> Result {
	if self.services.client.proxy.resolver_alias(url) {
		return Err!(Request(Forbidden(
			"Requesting a locally resolved proxy endpoint is forbidden"
		)));
	}

	let host = url
		.host()
		.ok_or_else(|| err!(Request(Unknown("URL has no host"))))?;

	let ip = match host {
		| Host::Domain(_) => return Ok(()),
		| Host::Ipv4(v4) => IpAddr::V4(v4),
		| Host::Ipv6(v6) => IpAddr::V6(v6),
	};

	if !self.services.client.valid_cidr_range_ip(ip) {
		return Err!(Request(Forbidden("Requesting from this address is forbidden")));
	}

	Ok(())
}

#[implement(Service)]
pub fn url_preview_allowed(&self, url: &Url) -> bool {
	if ["http", "https"]
		.iter()
		.all(|&scheme| !scheme.eq_ignore_ascii_case(url.scheme()))
	{
		debug!("Ignoring non-HTTP/HTTPS URL to preview: {}", url);
		return false;
	}

	let host = match url.host_str() {
		| None => {
			debug!("Ignoring URL preview for a URL that does not have a host (?): {}", url);
			return false;
		},
		| Some(h) => h.to_owned(),
	};

	let allowlist_domain_contains = &self
		.services
		.config
		.url_preview_domain_contains_allowlist;
	let allowlist_domain_explicit = &self
		.services
		.config
		.url_preview_domain_explicit_allowlist;
	let denylist_domain_explicit = &self
		.services
		.config
		.url_preview_domain_explicit_denylist;
	let allowlist_url_contains = &self
		.services
		.config
		.url_preview_url_contains_allowlist;

	if allowlist_domain_contains.contains(&"*".to_owned())
		|| allowlist_domain_explicit.contains(&"*".to_owned())
		|| allowlist_url_contains.contains(&"*".to_owned())
	{
		debug!("Config key contains * which is allowing all URL previews. Allowing URL {}", url);
		return true;
	}

	if !host.is_empty() {
		if denylist_domain_explicit.contains(&host) {
			debug!(
				"Host {} is not allowed by url_preview_domain_explicit_denylist (check 1/4)",
				&host
			);
			return false;
		}

		if allowlist_domain_explicit.contains(&host) {
			debug!(
				"Host {} is allowed by url_preview_domain_explicit_allowlist (check 2/4)",
				&host
			);
			return true;
		}

		if allowlist_domain_contains
			.iter()
			.any(|domain_s| domain_s.contains(&host.clone()))
		{
			debug!(
				"Host {} is allowed by url_preview_domain_contains_allowlist (check 3/4)",
				&host
			);
			return true;
		}

		if allowlist_url_contains
			.iter()
			.any(|url_s| url.to_string().contains(url_s))
		{
			debug!("URL {} is allowed by url_preview_url_contains_allowlist (check 4/4)", &host);
			return true;
		}

		// check root domain if available and if user has root domain checks
		if self.services.config.url_preview_check_root_domain {
			debug!("Checking root domain");
			match host.split_once('.') {
				| None => return false,
				| Some((_, root_domain)) => {
					if denylist_domain_explicit.contains(&root_domain.to_owned()) {
						debug!(
							"Root domain {} is not allowed by \
							 url_preview_domain_explicit_denylist (check 1/3)",
							&root_domain
						);
						return false;
					}

					if allowlist_domain_explicit.contains(&root_domain.to_owned()) {
						debug!(
							"Root domain {} is allowed by url_preview_domain_explicit_allowlist \
							 (check 2/3)",
							&root_domain
						);
						return true;
					}

					if allowlist_domain_contains
						.iter()
						.any(|domain_s| domain_s.contains(&root_domain.to_owned()))
					{
						debug!(
							"Root domain {} is allowed by url_preview_domain_contains_allowlist \
							 (check 3/3)",
							&root_domain
						);
						return true;
					}
				},
			}
		}
	}

	false
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use minicbor_serde::{from_slice, to_vec};
	use tuwunel_core::utils::time::timepoint_ago;
	use url::Url;

	use super::{CachedPreview, UrlPreviewData, is_youtube};
	#[cfg(feature = "url_preview")]
	use super::{oembed_endpoint, reserve_capped, video_type};

	const TTL: Duration = Duration::from_hours(24);

	fn sample() -> UrlPreviewData {
		UrlPreviewData {
			title: Some("Title".to_owned()),
			description: Some("Description".to_owned()),
			image: Some("mxc://example.org/image".to_owned()),
			// values carrying a 0xFF byte, which sheared fields in the old codec
			image_size: Some(0xFF01),
			image_width: Some(640),
			image_height: Some(0xFF),
			video: Some("mxc://example.org/video".to_owned()),
			video_type: Some("video/mp4".into()),
			video_size: Some(123_456),
			video_width: Some(1920),
			video_height: Some(1080),
			audio: Some("mxc://example.org/audio".to_owned()),
			audio_size: Some(4096),
			og_type: Some("website".to_owned()),
			og_url: Some("https://example.org/".to_owned()),
		}
	}

	#[test]
	fn cached_preview_roundtrip() {
		let cached = CachedPreview::new(TTL, sample()).expect("representable");
		let bytes = to_vec(&cached).expect("encodes");
		let decoded: CachedPreview = from_slice(&bytes).expect("decodes");

		assert_eq!(
			serde_json::to_value(&decoded.preview).expect("json"),
			serde_json::to_value(&cached.preview).expect("json"),
		);
		assert_eq!(decoded.preview.image_size, Some(0xFF01));
		assert_eq!(decoded.preview.image_height, Some(0xFF));
		assert_eq!(decoded.expire, cached.expire);
	}

	#[test]
	fn preview_wire_keys_unchanged() {
		let value = serde_json::to_value(sample()).expect("json");
		let object = value.as_object().expect("object");

		assert!(object.contains_key("og:title"));
		assert!(object.contains_key("matrix:image:size"));
		assert!(object.contains_key("og:video:width"));
		assert!(object.contains_key("og:video:type"));
		assert!(object.contains_key("og:url"));
		assert!(!object.contains_key("title"));

		let empty = serde_json::to_value(UrlPreviewData::default()).expect("json");
		assert!(empty.as_object().expect("object").is_empty());
	}

	#[test]
	fn preview_cbor_missing_fields_default() {
		let sparse = UrlPreviewData {
			title: Some("Only a title".to_owned()),
			..Default::default()
		};

		let bytes = to_vec(&sparse).expect("encodes");
		let decoded: UrlPreviewData = from_slice(&bytes).expect("decodes");

		assert_eq!(decoded.title.as_deref(), Some("Only a title"));
		assert!(decoded.description.is_none());
		assert!(decoded.image.is_none());
		assert!(decoded.og_url.is_none());
	}

	#[test]
	fn preview_cbor_unknown_key_skipped() {
		#[derive(serde::Serialize)]
		struct Superset {
			#[serde(rename = "og:title")]
			title: &'static str,
			#[serde(rename = "og:unknown")]
			unknown: &'static str,
		}

		let bytes = to_vec(Superset { title: "Kept", unknown: "Discarded" }).expect("encodes");
		let decoded: UrlPreviewData = from_slice(&bytes).expect("decodes");

		assert_eq!(decoded.title.as_deref(), Some("Kept"));
		assert!(decoded.description.is_none());
	}

	#[test]
	fn cached_preview_expiry() {
		let cached = CachedPreview::new(TTL, UrlPreviewData::default()).expect("representable");

		assert!(cached.valid());

		let expired = CachedPreview {
			preview: UrlPreviewData::default(),
			expire: timepoint_ago(Duration::from_secs(1)).expect("representable"),
		};

		assert!(!expired.valid());
	}

	#[test]
	fn cached_preview_honors_configured_lifetime() {
		let day = CachedPreview::new(TTL, UrlPreviewData::default()).expect("representable");
		let month =
			CachedPreview::new(TTL * 30, UrlPreviewData::default()).expect("representable");

		assert!(month.expire > day.expire);
	}

	#[test]
	fn cached_preview_unrepresentable_lifetime_refused() {
		let refused =
			CachedPreview::new(Duration::from_secs(u64::MAX), UrlPreviewData::default());

		assert!(refused.is_err(), "a lifetime past representable time errors rather than panics");
	}

	#[test]
	fn youtube_hosts_matched() {
		let youtube = [
			"https://www.youtube.com/watch?v=abc",
			"https://youtu.be/abc",
			"https://music.youtube.com/watch?v=abc",
			"https://m.youtube.com/watch?v=abc",
			"https://youtube.com/watch?v=abc",
			"https://WWW.YOUTUBE.COM/watch?v=abc",
		];

		for url in youtube {
			assert!(is_youtube(&Url::parse(url).expect("parses")), "{url}");
		}

		// a host merely ending in the domain is a different origin
		let other = [
			"https://youtube.com.evil.example/watch?v=abc",
			"https://notyoutube.com/watch?v=abc",
			"https://i.ytimg.com/vi/abc/hqdefault.jpg",
			"https://example.org/",
		];

		for url in other {
			assert!(!is_youtube(&Url::parse(url).expect("parses")), "{url}");
		}
	}

	#[cfg(feature = "url_preview")]
	#[test]
	fn oembed_endpoint_carries_the_page_url() {
		let url = Url::parse("https://www.youtube.com/watch?v=a&b=c").expect("parses");
		let endpoint = oembed_endpoint(&url).expect("youtube has an endpoint");

		assert_eq!(endpoint.path(), "/oembed");

		let params: Vec<_> = endpoint.query_pairs().collect();
		assert_eq!(params, [
			("url".into(), url.as_str().into()),
			("format".into(), "json".into())
		]);

		assert!(oembed_endpoint(&Url::parse("https://example.org/").expect("parses")).is_none());
	}

	#[cfg(feature = "url_preview")]
	#[test]
	fn oembed_video_declares_a_player() {
		assert_eq!(video_type(Some("video")), Some("text/html"));

		for kind in [Some("photo"), Some("rich"), Some("link"), None] {
			assert!(video_type(kind).is_none(), "{kind:?}");
		}
	}

	#[cfg(feature = "url_preview")]
	#[test]
	fn reserve_capped_never_exceeds_the_cap() {
		const LIMIT: usize = 768 * 1024;

		let mut bytes: Vec<u8> = Vec::new();
		let chunk = vec![0_u8; 16 * 1024];
		let mut reallocs = 0;

		while bytes.len() < LIMIT {
			let want = chunk.len().min(LIMIT.saturating_sub(bytes.len()));
			let before = bytes.capacity();

			reserve_capped(&mut bytes, want, LIMIT);
			bytes.extend_from_slice(&chunk[..want]);

			if bytes.capacity() != before {
				reallocs += 1;
			}

			assert!(bytes.capacity() <= LIMIT, "capacity {} past cap", bytes.capacity());
		}

		assert_eq!(bytes.len(), LIMIT);

		// geometric growth, not one reallocation per chunk
		assert!(reallocs < 12, "{reallocs} reallocations");
	}

	#[cfg(feature = "url_preview")]
	#[test]
	fn reserve_capped_honors_an_unclamped_request() {
		let mut bytes: Vec<u8> = Vec::new();

		reserve_capped(&mut bytes, 64, 16);

		assert!(bytes.capacity() >= 64);
	}
}
