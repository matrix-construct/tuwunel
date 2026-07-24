//! Tuwunel's client-IP extractor.
//!
//! Two modes:
//!
//! * If the operator configured `ip_source`, a [`ConfiguredIpSource`] marker is
//!   installed in request extensions and the extractor reads from the chosen
//!   source. Exception: if the peer shown by `ConnectInfo` is on a loopback
//!   interface, or sits inside an operator-listed trusted subnet (see
//!   [`TrustedPeerSubnets`]), the insecure header-scan + `ConnectInfo` fallback
//!   runs instead, since such peers (e.g. a locally-connected appservice
//!   bridge, or a containerized bridge on a private Docker network) cannot have
//!   spoofed the address at the IP layer.
//! * Otherwise the insecure header-scan + `ConnectInfo` fallback runs directly,
//!   preserving the prior default behaviour, including the socket-address
//!   fallback that matters for Unix-socket deployments.

use std::{
	fmt,
	net::{IpAddr, SocketAddr},
	sync::Arc,
};

use axum::extract::{ConnectInfo, FromRequestParts};
use http::{Extensions, HeaderMap, StatusCode, request::Parts};
use ipnet::IpNet;
use tuwunel_core::config::IpSource;

/// Tuwunel client-IP extractor. See module docs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClientIp(pub(crate) IpAddr);

#[derive(Clone, Copy, Debug)]
pub(crate) struct RateLimitIp(pub(crate) IpAddr);

/// Marker wrapper around [`IpSource`] placed into request extensions
/// only when an operator has explicitly configured `ip_source`.
#[derive(Clone, Copy, Debug)]
pub struct ConfiguredIpSource(pub IpSource);

/// Operator-configured subnets whose TCP peers bypass the secure
/// `ip_source` extraction in the same way loopback peers do. Installed
/// in request extensions only when the configured list is non-empty.
#[derive(Clone, Debug)]
pub struct TrustedPeerSubnets(pub Arc<[IpNet]>);

impl<S> FromRequestParts<S> for ClientIp
where
	S: Sync,
{
	type Rejection = (StatusCode, &'static str);

	async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
		const ERROR: StatusCode = StatusCode::INTERNAL_SERVER_ERROR;

		if let Some(&ConfiguredIpSource(source)) = parts.extensions.get::<ConfiguredIpSource>()
			&& !peer_is_trusted(&parts.extensions)
		{
			return secure_extract(source, &parts.headers, &parts.extensions)
				.map(Self)
				.ok_or((ERROR, "Can't extract client IP from configured ip_source"));
		}

		insecure_fallback(&parts.headers, &parts.extensions)
			.map(Self)
			.ok_or((ERROR, "Can't extract `ClientIp`, provide `axum::extract::ConnectInfo`"))
	}
}

impl<S> FromRequestParts<S> for RateLimitIp
where
	S: Sync,
{
	type Rejection = (StatusCode, &'static str);

	async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
		const ERROR: StatusCode = StatusCode::INTERNAL_SERVER_ERROR;

		if let Some(&ConfiguredIpSource(source)) = parts.extensions.get::<ConfiguredIpSource>() {
			return secure_extract(source, &parts.headers, &parts.extensions)
				.map(Self)
				.ok_or((ERROR, "Can't extract rate-limit IP from configured ip_source"));
		}

		parts
			.extensions
			.get::<ConnectInfo<SocketAddr>>()
			.map(|ConnectInfo(addr)| Self(addr.ip()))
			.ok_or((ERROR, "Can't extract `RateLimitIp`, provide `axum::extract::ConnectInfo`"))
	}
}

impl fmt::Display for ClientIp {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Display::fmt(&self.0, f) }
}

fn peer_is_trusted(extensions: &Extensions) -> bool {
	let Some(ConnectInfo(addr)) = extensions.get::<ConnectInfo<SocketAddr>>() else {
		return false;
	};

	let peer = addr.ip().to_canonical();

	peer.is_loopback()
		|| extensions
			.get::<TrustedPeerSubnets>()
			.is_some_and(|TrustedPeerSubnets(nets)| nets.iter().any(|net| net.contains(&peer)))
}

fn secure_extract(
	source: IpSource,
	headers: &HeaderMap,
	extensions: &Extensions,
) -> Option<IpAddr> {
	match source {
		| IpSource::ConnectInfo => extensions
			.get::<ConnectInfo<SocketAddr>>()
			.map(|ConnectInfo(addr)| addr.ip()),
		| IpSource::RightmostXForwardedFor => rightmost_x_forwarded_for(headers),
		| IpSource::RightmostForwarded => rightmost_forwarded(headers),
		| IpSource::XRealIp => single_ip_header(headers, "x-real-ip"),
		| IpSource::CfConnectingIp => single_ip_header(headers, "cf-connecting-ip"),
		| IpSource::TrueClientIp => single_ip_header(headers, "true-client-ip"),
		| IpSource::FlyClientIp => single_ip_header(headers, "fly-client-ip"),
		| IpSource::CloudFrontViewerAddress => cloudfront_viewer_address(headers),
	}
}

fn rightmost_x_forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
	headers
		.get_all("x-forwarded-for")
		.iter()
		.filter_map(|v| v.to_str().ok())
		.flat_map(|s| s.split(','))
		.filter_map(|s| s.trim().parse::<IpAddr>().ok())
		.next_back()
}

fn rightmost_forwarded(headers: &HeaderMap) -> Option<IpAddr> {
	headers
		.get_all("forwarded")
		.iter()
		.filter_map(|v| v.to_str().ok())
		.flat_map(|s| s.split(','))
		.filter_map(parse_forwarded_for)
		.next_back()
}

/// Leftmost header scan with `ConnectInfo` fallback.
fn insecure_fallback(headers: &HeaderMap, extensions: &Extensions) -> Option<IpAddr> {
	leftmost_x_forwarded_for(headers)
		.or_else(|| leftmost_forwarded(headers))
		.or_else(|| single_ip_header(headers, "x-real-ip"))
		.or_else(|| single_ip_header(headers, "fly-client-ip"))
		.or_else(|| single_ip_header(headers, "true-client-ip"))
		.or_else(|| single_ip_header(headers, "cf-connecting-ip"))
		.or_else(|| cloudfront_viewer_address(headers))
		.or_else(|| {
			extensions
				.get::<ConnectInfo<SocketAddr>>()
				.map(|ConnectInfo(addr)| addr.ip())
		})
}

fn leftmost_x_forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
	headers
		.get_all("x-forwarded-for")
		.iter()
		.filter_map(|v| v.to_str().ok())
		.flat_map(|s| s.split(','))
		.find_map(|s| s.trim().parse::<IpAddr>().ok())
}

/// Parse `for=` from the leftmost RFC 7239 stanza. Tolerates quoted
/// values, bracketed IPv6, and an optional `:port` suffix.
fn leftmost_forwarded(headers: &HeaderMap) -> Option<IpAddr> {
	headers
		.get_all("forwarded")
		.iter()
		.filter_map(|v| v.to_str().ok())
		.flat_map(|s| s.split(','))
		.find_map(parse_forwarded_for)
}

fn parse_forwarded_for(stanza: &str) -> Option<IpAddr> {
	let for_value = stanza
		.split(';')
		.find_map(|part| {
			let (k, v) = part.split_once('=')?;
			k.trim()
				.eq_ignore_ascii_case("for")
				.then_some(v.trim())
		})?
		.trim_matches('"');

	let bracketed = for_value
		.strip_prefix('[')
		.and_then(|s| s.split_once(']'))
		.map(|(ip, _rest)| ip);

	let candidate = bracketed
		.or_else(|| for_value.rsplit_once(':').map(|(ip, _port)| ip))
		.unwrap_or(for_value);

	candidate.trim().parse::<IpAddr>().ok()
}

fn single_ip_header(headers: &HeaderMap, name: &'static str) -> Option<IpAddr> {
	headers
		.get(name)
		.and_then(|v| v.to_str().ok())
		.and_then(|s| s.trim().parse::<IpAddr>().ok())
}

fn cloudfront_viewer_address(headers: &HeaderMap) -> Option<IpAddr> {
	headers
		.get("cloudfront-viewer-address")
		.and_then(|v| v.to_str().ok())
		.and_then(|s| s.rsplit_once(':').map(|(ip, _port)| ip))
		.and_then(|s| s.trim().parse::<IpAddr>().ok())
}

#[cfg(test)]
mod tests {
	use std::{
		iter,
		net::{IpAddr, SocketAddr},
		sync::Arc,
	};

	use axum::{
		extract::{ConnectInfo, FromRequestParts},
		http::{Request, StatusCode, request::Parts},
	};
	use ipnet::IpNet;
	use tuwunel_core::config::IpSource;

	use super::{ClientIp, ConfiguredIpSource, RateLimitIp, TrustedPeerSubnets};

	fn trusted(nets: &[&str]) -> TrustedPeerSubnets {
		let nets: Arc<[IpNet]> = nets
			.iter()
			.map(|s| s.parse().expect("test CIDR"))
			.collect();

		TrustedPeerSubnets(nets)
	}

	fn parts(headers: impl IntoIterator<Item = (&'static str, &'static str)>) -> Parts {
		let mut request = Request::builder().uri("/");
		for (name, value) in headers {
			request = request.header(name, value);
		}
		let (parts, ()) = request.body(()).unwrap().into_parts();
		parts
	}

	async fn extract_client_ip(
		parts: &mut Parts,
	) -> Result<ClientIp, (StatusCode, &'static str)> {
		ClientIp::from_request_parts(parts, &()).await
	}

	async fn extract_rate_limit_ip(
		parts: &mut Parts,
	) -> Result<RateLimitIp, (StatusCode, &'static str)> {
		RateLimitIp::from_request_parts(parts, &()).await
	}

	#[tokio::test]
	async fn x_forwarded_for_uses_leftmost_ip() {
		let mut parts = parts([("X-Forwarded-For", "1.1.1.1, 2.2.2.2")]);
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "1.1.1.1");
	}

	#[tokio::test]
	async fn x_forwarded_for_takes_priority_over_x_real_ip() {
		let mut parts =
			parts([("X-Forwarded-For", "1.1.1.1, 2.2.2.2"), ("X-Real-Ip", "3.3.3.3")]);
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "1.1.1.1");
	}

	#[tokio::test]
	async fn x_forwarded_for_accepts_ipv6() {
		let mut parts = parts([("X-Forwarded-For", "2001:db8::1, 2001:db8::2")]);
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "2001:db8::1");
	}

	#[tokio::test]
	async fn x_real_ip_works() {
		let mut parts = parts([("X-Real-Ip", "1.2.3.4")]);
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "1.2.3.4");
	}

	#[tokio::test]
	async fn malformed_headers_fall_through_to_next_valid_source() {
		let mut parts = parts([
			("X-Forwarded-For", "foo"),
			("X-Real-Ip", "foo"),
			("Forwarded", "foo"),
			("Forwarded", "for=1.1.1.1;proto=https;by=2.2.2.2"),
		]);
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "1.1.1.1");
	}

	#[tokio::test]
	async fn no_headers_or_connect_info_rejects() {
		let mut parts = parts(iter::empty());
		let err = extract_client_ip(&mut parts).await.unwrap_err();
		assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
		assert!(err.1.contains("ConnectInfo"), "{err:?}");
	}

	#[tokio::test]
	async fn configured_source_uses_secure_extraction() {
		let mut parts = parts([("X-Forwarded-For", "1.1.1.1, 2.2.2.2")]);
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "2.2.2.2");
	}

	#[tokio::test]
	async fn rate_limit_ip_ignores_unconfigured_headers_and_trusted_peers() {
		let socket_addr = SocketAddr::from(([203, 0, 113, 9], 4567));

		for forwarded in ["1.1.1.1", "2.2.2.2"] {
			let mut parts = parts([("X-Forwarded-For", forwarded)]);

			parts.extensions.insert(ConnectInfo(socket_addr));

			let RateLimitIp(ip) = extract_rate_limit_ip(&mut parts).await.unwrap();

			assert_eq!(ip, socket_addr.ip());
		}

		let socket_addr = SocketAddr::from(([172, 18, 0, 5], 38000));
		let mut parts = parts([("X-Forwarded-For", "1.1.1.1, 2.2.2.2")]);

		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));

		parts
			.extensions
			.insert(trusted(&["172.18.0.0/16"]));

		let RateLimitIp(ip) = extract_rate_limit_ip(&mut parts).await.unwrap();

		assert_eq!(ip, IpAddr::from([2, 2, 2, 2]));
	}

	#[tokio::test]
	async fn configured_source_without_matching_header_rejects() {
		let mut parts = parts(iter::empty());
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));
		let err = extract_client_ip(&mut parts).await.unwrap_err();
		assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
		assert_eq!(err.1, "Can't extract client IP from configured ip_source");
	}

	#[tokio::test]
	async fn connect_info_fallback_uses_real_socket_addr_without_config() {
		let socket_addr = SocketAddr::from(([203, 0, 113, 9], 4567));
		let mut parts = parts(iter::empty());
		parts.extensions.insert(ConnectInfo(socket_addr));

		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, socket_addr.ip());
	}

	#[tokio::test]
	async fn loopback_peer_bypasses_configured_source_for_locally_connected_bridges() {
		let socket_addr = SocketAddr::from(([127, 0, 0, 1], 38000));
		let mut parts = parts(iter::empty());
		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));

		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, socket_addr.ip());
	}

	#[tokio::test]
	async fn loopback_peer_with_proxy_header_still_uses_insecure_fallback() {
		// When a loopback peer also forwards a proxy header (e.g. a local
		// reverse proxy in a sidecar), the insecure leftmost-XFF behaviour wins
		// over the loopback ConnectInfo fallback, matching how the unconfigured
		// path already behaves.
		let socket_addr = SocketAddr::from(([127, 0, 0, 1], 38000));
		let mut parts = parts([("X-Forwarded-For", "9.9.9.9")]);
		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));

		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "9.9.9.9");
	}

	#[tokio::test]
	async fn ipv6_loopback_peer_also_bypasses_configured_source() {
		let socket_addr = SocketAddr::from(([0_u16, 0, 0, 0, 0, 0, 0, 1], 38000));
		let mut parts = parts(iter::empty());
		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));

		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, socket_addr.ip());
	}

	#[tokio::test]
	async fn non_loopback_peer_with_configured_source_still_rejects() {
		let socket_addr = SocketAddr::from(([203, 0, 113, 9], 38000));
		let mut parts = parts(iter::empty());
		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));

		let err = extract_client_ip(&mut parts).await.unwrap_err();
		assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
		assert_eq!(err.1, "Can't extract client IP from configured ip_source");
	}

	#[tokio::test]
	async fn trusted_subnet_peer_bypasses_configured_source() {
		let socket_addr = SocketAddr::from(([172, 18, 0, 5], 38000));
		let mut parts = parts(iter::empty());
		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));
		parts
			.extensions
			.insert(trusted(&["172.18.0.0/16"]));

		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, socket_addr.ip());
	}

	#[tokio::test]
	async fn trusted_subnet_peer_with_proxy_header_uses_insecure_fallback() {
		let socket_addr = SocketAddr::from(([172, 18, 0, 5], 38000));
		let mut parts = parts([("X-Forwarded-For", "9.9.9.9")]);
		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));
		parts
			.extensions
			.insert(trusted(&["172.18.0.0/16"]));

		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "9.9.9.9");
	}

	#[tokio::test]
	async fn non_trusted_peer_with_subnets_configured_still_rejects() {
		let socket_addr = SocketAddr::from(([203, 0, 113, 9], 38000));
		let mut parts = parts(iter::empty());
		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));
		parts
			.extensions
			.insert(trusted(&["172.18.0.0/16"]));

		let err = extract_client_ip(&mut parts).await.unwrap_err();
		assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
		assert_eq!(err.1, "Can't extract client IP from configured ip_source");
	}

	#[tokio::test]
	async fn ipv6_trusted_subnet_peer_bypasses_configured_source() {
		let socket_addr = SocketAddr::from(([0xFD00_u16, 0, 0, 0, 0, 0, 0, 1], 38000));
		let mut parts = parts(iter::empty());
		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));
		parts.extensions.insert(trusted(&["fd00::/8"]));

		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, socket_addr.ip());
	}

	#[tokio::test]
	async fn trusted_single_host_cidr_matches_only_that_address() {
		let configured = ConfiguredIpSource(IpSource::RightmostXForwardedFor);

		let mut listed = parts(iter::empty());
		listed
			.extensions
			.insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 5], 38000))));
		listed.extensions.insert(configured);
		listed
			.extensions
			.insert(trusted(&["10.0.0.5/32"]));

		let ClientIp(ip) = extract_client_ip(&mut listed).await.unwrap();
		assert_eq!(ip.to_string(), "10.0.0.5");

		let mut neighbour = parts(iter::empty());
		neighbour
			.extensions
			.insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 6], 38000))));
		neighbour.extensions.insert(configured);
		neighbour
			.extensions
			.insert(trusted(&["10.0.0.5/32"]));

		let err = extract_client_ip(&mut neighbour)
			.await
			.unwrap_err();
		assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
	}

	#[tokio::test]
	async fn loopback_still_bypasses_when_trusted_subnets_extension_absent() {
		let socket_addr = SocketAddr::from(([127, 0, 0, 1], 38000));
		let mut parts = parts(iter::empty());
		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(ConfiguredIpSource(IpSource::RightmostXForwardedFor));

		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, socket_addr.ip());
	}
}
