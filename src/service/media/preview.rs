//! URL Previews
//!
//! This functionality is gated by 'url_preview', but not at the unit level for
//! historical and simplicity reasons. Instead the feature gates the inclusion
//! of dependencies and nulls out results through the existing interface when
//! not featured.

use std::time::SystemTime;

use ipaddress::IPAddress;
use serde::Serialize;
use tuwunel_core::{Err, Result, debug, err, implement};
use url::Url;

use super::Service;

#[derive(Default, Serialize)]
pub struct UrlPreviewData {
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:title")
	)]
	pub title: Option<String>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:description")
	)]
	pub description: Option<String>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:image")
	)]
	pub image: Option<String>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "matrix:image:size")
	)]
	pub image_size: Option<usize>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:image:width")
	)]
	pub image_width: Option<u32>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:image:height")
	)]
	pub image_height: Option<u32>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:video")
	)]
	pub video: Option<String>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "matrix:video:size")
	)]
	pub video_size: Option<usize>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:video:width")
	)]
	pub video_width: Option<u32>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:video:height")
	)]
	pub video_height: Option<u32>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:audio")
	)]
	pub audio: Option<String>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "matrix:audio:size")
	)]
	pub audio_size: Option<usize>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:type")
	)]
	pub og_type: Option<String>,
	#[serde(
		skip_serializing_if = "Option::is_none",
		rename(serialize = "og:url")
	)]
	pub og_url: Option<String>,
}

#[implement(Service)]
pub fn remove_url_preview(&self, url: &str) -> Result {
	// TODO: also remove the downloaded image
	self.db.remove_url_preview(url)
}

#[implement(Service)]
pub fn set_url_preview(&self, url: &str, data: &UrlPreviewData) -> Result {
	let now = SystemTime::now()
		.duration_since(SystemTime::UNIX_EPOCH)
		.expect("valid system time");
	self.db.set_url_preview(url, data, now)
}

#[implement(Service)]
pub async fn get_url_preview(&self, url: &Url) -> Result<UrlPreviewData> {
	if let Ok(preview) = self.db.get_url_preview(url.as_str()).await {
		return Ok(preview);
	}

	// ensure that only one request is made per URL
	let _request_lock = self.url_preview_mutex.lock(url.as_str()).await;

	match self.db.get_url_preview(url.as_str()).await {
		| Ok(preview) => Ok(preview),
		| Err(_) => self.request_url_preview(url).await,
	}
}

#[implement(Service)]
pub async fn request_url_preview(&self, url: &Url) -> Result<UrlPreviewData> {
	if let Ok(ip) = IPAddress::parse(url.host_str().expect("URL previously validated"))
		&& !self.services.client.valid_cidr_range(&ip)
	{
		return Err!(Request(Forbidden("Requesting from this address is forbidden")));
	}

	let client = &self.services.client.url_preview;
	let response = client.get(url.as_str()).send().await?;

	debug!(?url, "URL preview response headers: {:?}", response.headers());

	if let Some(remote_addr) = response.remote_addr() {
		debug!(?url, "URL preview response remote address: {:?}", remote_addr);

		if let Ok(ip) = IPAddress::parse(remote_addr.ip().to_string())
			&& !self.services.client.valid_cidr_range(&ip)
		{
			return Err!(Request(Forbidden("Requesting from this address is forbidden")));
		}
	}

	let content_type = response
		.headers()
		.get(reqwest::header::CONTENT_TYPE)
		.ok_or_else(|| err!(Request(Unknown("Missing Content-Type header"))))?
		.to_str()
		.map_err(|e| err!(Request(Unknown("Invalid Content-Type header: {e}"))))?
		.to_owned();

	let data = match content_type.as_str() {
		| html if html.starts_with("text/html") => self.download_html(url, response).await?,
		| img if img.starts_with("image/") => self.download_image(response).await?,
		| _ => return Err!(Request(Unknown("Unsupported Content-Type"))),
	};

	self.set_url_preview(url.as_str(), &data)?;

	Ok(data)
}

#[cfg(feature = "url_preview")]
#[implement(Service)]
pub async fn download_image(&self, response: reqwest::Response) -> Result<UrlPreviewData> {
	use image::ImageReader;
	use ruma::Mxc;
	use tuwunel_core::utils::random_string;

	let image = response.bytes().await?;
	let mxc = Mxc {
		server_name: self.services.globals.server_name(),
		media_id: &random_string(super::MXC_LENGTH),
	};

	self.create(&mxc, None, None, None, &image)
		.await?;

	let cursor = std::io::Cursor::new(&image);
	let (width, height) = match ImageReader::new(cursor).with_guessed_format() {
		| Err(_) => (None, None),
		| Ok(reader) => match reader.into_dimensions() {
			| Err(_) => (None, None),
			| Ok((width, height)) => (Some(width), Some(height)),
		},
	};

	Ok(UrlPreviewData {
		image: Some(mxc.to_string()),
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
	let client = &self.services.client.url_preview;
	let mut data = match html.opengraph.images.first() {
		| None => UrlPreviewData::default(),
		| Some(obj) => {
			let image_url = url
				.join(&obj.url)
				.map_err(|e| err!(Request(Unknown("Invalid og:image URL: {e}"))))?;
			let image_response = client.get(image_url.as_str()).send().await?;
			self.download_image(image_response).await?
		},
	};

	let props = html.opengraph.properties;

	/* use OpenGraph title/description, but fall back to HTML if not available */
	data.title = props.get("title").cloned().or(html.title);
	data.description = props
		.get("description")
		.cloned()
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
						return true;
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
