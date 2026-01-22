use reqwest::{Proxy, Url};
use serde::Deserialize;

use crate::Result;

/// ## Examples:
/// - No proxy (default):
/// ```toml
/// proxy ="none"
/// ```
/// - Global proxy
/// ```toml
/// [global.proxy]
/// global = { url = "socks5h://localhost:9050" }
/// ```
/// - Proxy some domains
/// ```toml
/// [global.proxy]
/// [[global.proxy.by_domain]]
/// url = "socks5h://localhost:9050"
/// include = ["*.onion", "matrix.myspecial.onion"]
/// exclude = ["*.myspecial.onion"]
/// ```
/// ## Include vs. Exclude
/// If include is an empty list, it is assumed to be `["*"]`.
///
/// If a domain matches both the exclude and include list, the proxy will only
/// be used if it was included because of a more specific rule than it was
/// excluded. In the above example, the proxy would be used for
/// `ordinary.onion`, `matrix.myspecial.onion`, but not `hello.myspecial.onion`.
#[derive(Clone, Default, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyConfig {
	#[default]
	None,
	Global {
		#[serde(deserialize_with = "crate::utils::deserialize_from_str")]
		url: Url,
	},
	ByDomain(Vec<PartialProxyConfig>),
}
impl ProxyConfig {
	pub fn to_proxy(&self) -> Result<Option<Proxy>> {
		Ok(match self.clone() {
			| Self::None => None,
			| Self::Global { url } => Some(Proxy::all(url)?),
			| Self::ByDomain(proxies) => Some(Proxy::custom(move |url| {
				// first matching proxy
				proxies
					.iter()
					.find_map(|proxy| proxy.for_url(url))
					.cloned()
			})),
		})
	}
}

#[derive(Clone, Debug, Deserialize)]
pub struct PartialProxyConfig {
	#[serde(deserialize_with = "crate::utils::deserialize_from_str")]
	url: Url,
	#[serde(default)]
	include: Vec<WildCardedDomain>,
	#[serde(default)]
	exclude: Vec<WildCardedDomain>,
}
impl PartialProxyConfig {
	#[must_use]
	pub fn for_url(&self, url: &Url) -> Option<&Url> {
		let domain = url.domain()?;
		let mut included_because = None; // most specific reason it was included
		let mut excluded_because = None; // most specific reason it was excluded
		if self.include.is_empty() {
			// treat empty include list as `*`
			included_because = Some(&WildCardedDomain::WildCard);
		}
		for wc_domain in &self.include {
			if wc_domain.matches(domain) {
				match included_because {
					| Some(prev) if !wc_domain.more_specific_than(prev) => (),
					| _ => included_because = Some(wc_domain),
				}
			}
		}
		for wc_domain in &self.exclude {
			if wc_domain.matches(domain) {
				match excluded_because {
					| Some(prev) if !wc_domain.more_specific_than(prev) => (),
					| _ => excluded_because = Some(wc_domain),
				}
			}
		}
		match (included_because, excluded_because) {
			| (Some(a), Some(b)) if a.more_specific_than(b) => Some(&self.url),
			| (Some(_), None) => Some(&self.url),
			| _ => None,
		}
	}
}

/// A domain name, that optionally allows a * as its first subdomain.
#[derive(Clone, Debug)]
enum WildCardedDomain {
	WildCard,
	WildCarded(String),
	Exact(String),
}
impl WildCardedDomain {
	fn matches(&self, domain: &str) -> bool {
		match self {
			| Self::WildCard => true,
			| Self::WildCarded(d) => domain.ends_with(d),
			| Self::Exact(d) => domain == d,
		}
	}

	fn more_specific_than(&self, other: &Self) -> bool {
		match (self, other) {
			| (Self::WildCard, Self::WildCard) => false,
			| (_, Self::WildCard) => true,
			| (Self::Exact(a), Self::WildCarded(_)) => other.matches(a),
			| (Self::WildCarded(a), Self::WildCarded(b)) => a != b && a.ends_with(b),
			| _ => false,
		}
	}
}
impl std::str::FromStr for WildCardedDomain {
	type Err = std::convert::Infallible;

	#[expect(clippy::string_slice)]
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		// maybe do some domain validation?
		Ok(if s.starts_with("*.") {
			Self::WildCarded(s[1..].to_owned())
		} else if s == "*" {
			Self::WildCarded(String::new())
		} else {
			Self::Exact(s.to_owned())
		})
	}
}
impl<'de> Deserialize<'de> for WildCardedDomain {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::de::Deserializer<'de>,
	{
		crate::utils::deserialize_from_str(deserializer)
	}
}
