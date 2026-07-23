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
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use tuwunel_core::{Err, Result, debug, err, implement, utils::time::timepoint_from_now};
use url::{Host, Url};

use super::Service;

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
	// refetch daily; og metadata drifts
	const EXPIRE: Duration = Duration::from_hours(24);

	fn new(preview: UrlPreviewData) -> Self {
		let expire = timepoint_from_now(Self::EXPIRE).expect("1 day from now is representable");

		Self { preview, expire }
	}

	#[inline]
	#[must_use]
	pub(super) fn valid(&self) -> bool { self.expire > SystemTime::now() }
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

	let client = &self.services.client.url_preview;
	let response = client.get(url.as_str()).send().await?;

	debug!(?url, "URL preview response headers: {:?}", response.headers());

	let Some(remote_addr) = response.remote_addr() else {
		return Err!(Request(Forbidden("URL preview response has no peer address")));
	};

	debug!(?url, ?remote_addr, "URL preview response remote address");

	if !self
		.services
		.client
		.valid_cidr_range_ip(remote_addr.ip())
	{
		return Err!(Request(Forbidden("Requesting from this address is forbidden")));
	}

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

			self.download_html(url, response).await?
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

	let cached = CachedPreview::new(data);
	self.db.set_url_preview(url.as_str(), &cached)?;

	Ok(cached.preview)
}

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
	let image = crate::client::read_response_capped(response, limit).await?;

	let cursor = std::io::Cursor::new(&image);
	let (width, height) = match ImageReader::new(cursor).with_guessed_format() {
		| Err(_) => (None, None),
		| Ok(reader) => match reader.into_dimensions() {
			| Err(_) => (None, None),
			| Ok((width, height)) => (Some(width), Some(height)),
		},
	};

	let mxc = self.register_lazy_media(url.as_str());
	self.db.set_lazy_content(
		&mxc,
		content_type.as_deref(),
		content_disposition.as_deref(),
		&image,
	);

	Ok(UrlPreviewData {
		image: Some(mxc),
		image_size: Some(image.len()),
		image_width: width,
		image_height: height,
		..Default::default()
	})
}

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
	let client = &self.services.client.url_preview_media;
	let response = client.get(url.as_str()).send().await?;

	let Some(remote_addr) = response.remote_addr() else {
		return Err!(Request(Forbidden("URL preview media response has no peer address")));
	};

	if !self
		.services
		.client
		.valid_cidr_range_ip(remote_addr.ip())
	{
		return Err!(Request(Forbidden("Requesting from this address is forbidden")));
	}

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
	use ruma::Mxc;
	use tuwunel_core::utils::random_string;

	let mxc = Mxc {
		server_name: self.services.globals.server_name(),
		media_id: &random_string(super::MXC_LENGTH),
	};

	self.db.insert_lazy_media(&mxc, url);

	mxc.to_string()
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
async fn download_html(
	&self,
	url: &Url,
	mut response: reqwest::Response,
) -> Result<UrlPreviewData> {
	use webpage::HTML;

	let mut bytes: Vec<u8> = Vec::new();
	while let Some(chunk) = response.chunk().await? {
		bytes.extend_from_slice(&chunk);
		if bytes.len() > self.services.config.url_preview_max_spider_size {
			debug!(
				"Response body from URL {} exceeds url_preview_max_spider_size ({}), not \
				 processing the rest of the response body and assuming our necessary data is in \
				 this range.",
				url, self.services.config.url_preview_max_spider_size
			);
			break;
		}
	}
	let body = String::from_utf8_lossy(&bytes);
	let Ok(html) = HTML::from_string(body.to_string(), Some(url.to_string())) else {
		return Err!(Request(Unknown("Failed to parse HTML")));
	};

	// `webpage` does not resolve relative URLs in `og:` meta tags; resolve
	// against the page URL so e.g. `og:image=test.png` becomes absolute.
	//
	// the measurement fetch is a media fetch: it must use the same client
	// as the relay, or the origin could serve the measurement different
	// content than the relayed mxc.
	let client = &self.services.client.url_preview_media;

	// twitter:* card tags mirror og:; some pages emit only the twitter set,
	// or (fixvx) an empty og: value beside the real twitter: one
	let twitter = |key| {
		html.meta
			.get(key)
			.map(String::as_str)
			.filter(|content| !content.is_empty())
	};

	// only http(s) image URLs are fetchable; others keep the textual preview
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
		| Some(image_url) => {
			self.check_url_host(&image_url)?;
			let image_response = client.get(image_url.as_str()).send().await?;

			let Some(remote_addr) = image_response.remote_addr() else {
				return Err!(Request(Forbidden("preview image response has no peer address")));
			};

			debug!(?image_url, ?remote_addr, "preview image remote address");

			if !self
				.services
				.client
				.valid_cidr_range_ip(remote_addr.ip())
			{
				return Err!(Request(Forbidden("Requesting from this address is forbidden")));
			}

			// a failing preview image must not become a preview mxc the relay is
			// guaranteed to reject; skip it and keep the textual preview
			if image_response.status().is_success() {
				self.download_image(image_response).await?
			} else {
				debug!(
					?image_url,
					status = ?image_response.status(),
					"Skipping preview image with unsuccessful response"
				);

				UrlPreviewData::default()
			}
		},
	};

	// og:video/og:audio are registered as lazy media (see register_lazy_media)
	// rather than fetched here, so a page with a large og:video never costs a
	// preview request any bandwidth; the URL is only fetched, SSRF-checked,
	// and relayed when a client asks for the resulting mxc:// URI.
	// check_url_host screens IP-literal URLs here only so a preview never
	// hands out an mxc that the same check at relay time is guaranteed to
	// reject.
	if let Some(obj) = html.opengraph.videos.first()
		&& !obj.url.is_empty()
		&& let Ok(video_url) = url.join(&obj.url)
		&& ["http", "https"].contains(&video_url.scheme())
		&& self.check_url_host(&video_url).is_ok()
	{
		data.video = Some(self.register_lazy_media(video_url.as_str()));
		data.video_width = obj
			.properties
			.get("width")
			.and_then(|w| w.parse().ok());
		data.video_height = obj
			.properties
			.get("height")
			.and_then(|h| h.parse().ok());
	}

	if let Some(obj) = html.opengraph.audios.first()
		&& !obj.url.is_empty()
		&& let Ok(audio_url) = url.join(&obj.url)
		&& ["http", "https"].contains(&audio_url.scheme())
		&& self.check_url_host(&audio_url).is_ok()
	{
		data.audio = Some(self.register_lazy_media(audio_url.as_str()));
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

#[implement(Service)]
pub(super) fn check_url_host(&self, url: &Url) -> Result {
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
	use std::time::{Duration, SystemTime};

	use minicbor_serde::{from_slice, to_vec};

	use super::{CachedPreview, UrlPreviewData};

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
		let cached = CachedPreview::new(sample());
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
		let mut cached = CachedPreview::new(UrlPreviewData::default());
		assert!(cached.valid());

		cached.expire = SystemTime::now() - Duration::from_secs(1);
		assert!(!cached.valid());
	}
}
