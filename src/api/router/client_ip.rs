//! Tuwunel's client-IP extractor.
//!
//! Wraps `axum_client_ip` with a two-mode fallback:
//!
//! * If the operator configured `ip_source`, a [`ConfiguredIpSource`] marker is
//!   installed in request extensions and we delegate to
//!   [`axum_client_ip::SecureClientIp`] with that source.
//! * Otherwise we fall back to [`axum_client_ip::InsecureClientIp`], preserving
//!   existing behavior exactly -- including the header scan chain and the
//!   socket-address fallback that matters for Unix-socket deployments (see
//!   matrix-construct/tuwunel#310).
//!
//! The plain `SecureClientIpSource::ConnectInfo` extension already
//! installed by `src/router/layers.rs` is intentionally ignored here;
//! only the [`ConfiguredIpSource`] marker participates in the secure
//! path. This avoids flipping behavior for deployments that never opted
//! in.

use std::{fmt, marker::Sync, net::IpAddr};

use axum::extract::FromRequestParts;
use axum_client_ip::{InsecureClientIp, SecureClientIp, SecureClientIpSource};
use http::{StatusCode, request::Parts};

/// Tuwunel client-IP extractor. See module docs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClientIp(pub(crate) IpAddr);

/// Marker wrapper around [`SecureClientIpSource`] placed into request
/// extensions only when an operator has explicitly configured
/// `ip_source`.
#[derive(Clone, Debug)]
struct ConfiguredIpSource(SecureClientIpSource);

impl<S> FromRequestParts<S> for ClientIp
where
	S: Sync,
{
	type Rejection = (StatusCode, &'static str);

	async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
		const ERROR: StatusCode = StatusCode::INTERNAL_SERVER_ERROR;

		if let Some(ConfiguredIpSource(source)) = parts.extensions.get::<ConfiguredIpSource>() {
			SecureClientIp::from(source, &parts.headers, &parts.extensions)
				.map(|SecureClientIp(ip)| Self(ip))
				.map_err(|_| (ERROR, "Can't extract client IP from configured ip_source"))
		} else {
			InsecureClientIp::from(&parts.headers, &parts.extensions)
				.map(|InsecureClientIp(ip)| Self(ip))
		}
	}
}

impl fmt::Display for ClientIp {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Display::fmt(&self.0, f) }
}

#[cfg(test)]
mod tests {
	use std::net::SocketAddr;

	use axum::{
		extract::{ConnectInfo, FromRequestParts},
		http::{Request, StatusCode, request::Parts},
	};
	use axum_client_ip::SecureClientIpSource;

	use super::{ClientIp, ConfiguredIpSource};

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
		let mut parts = parts(std::iter::empty());
		let err = extract_client_ip(&mut parts).await.unwrap_err();
		assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
		assert!(err.1.contains("ConnectInfo"), "{err:?}");
	}

	#[tokio::test]
	async fn configured_source_uses_secure_extraction() {
		let mut parts = parts([("X-Forwarded-For", "1.1.1.1, 2.2.2.2")]);
		parts
			.extensions
			.insert(ConfiguredIpSource(SecureClientIpSource::RightmostXForwardedFor));
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "2.2.2.2");
	}

	#[tokio::test]
	async fn configured_source_without_matching_header_rejects() {
		let mut parts = parts(std::iter::empty());
		parts
			.extensions
			.insert(ConfiguredIpSource(SecureClientIpSource::RightmostXForwardedFor));
		let err = extract_client_ip(&mut parts).await.unwrap_err();
		assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
		assert_eq!(err.1, "Can't extract client IP from configured ip_source");
	}

	#[tokio::test]
	async fn secure_client_ip_source_extension_does_not_hijack() {
		let mut parts = parts([("X-Forwarded-For", "1.1.1.1, 2.2.2.2")]);
		parts
			.extensions
			.insert(SecureClientIpSource::ConnectInfo);
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "1.1.1.1");
	}

	#[tokio::test]
	async fn connect_info_fallback_uses_real_socket_addr_without_config() {
		let socket_addr = SocketAddr::from(([203, 0, 113, 9], 4567));
		let mut parts = parts(std::iter::empty());
		parts.extensions.insert(ConnectInfo(socket_addr));

		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, socket_addr.ip());
	}

	#[tokio::test]
	async fn bare_secure_client_ip_source_connect_info_does_not_hijack() {
		let socket_addr = SocketAddr::from(([203, 0, 113, 10], 4567));
		let mut parts = parts([("X-Forwarded-For", "1.1.1.1, 2.2.2.2")]);
		parts.extensions.insert(ConnectInfo(socket_addr));
		parts
			.extensions
			.insert(SecureClientIpSource::ConnectInfo);

		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "1.1.1.1");
	}
}
