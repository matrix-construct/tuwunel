//! Defines sources for trusted client IP extraction.
//!
//! [`IpSource`] enumerates the TCP peer address and supported forwarding
//! headers. The default uses the TCP peer address without trusting a proxy.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde::Deserialize;

use crate::implement;

/// Selects the source used to determine the connecting client's IP
/// address.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum IpSource {
	/// TCP peer address. Safe default; no proxy required.
	#[default]
	ConnectInfo,

	/// Rightmost value of `X-Forwarded-For`.
	RightmostXForwardedFor,

	/// Rightmost value of RFC 7239 `Forwarded`.
	RightmostForwarded,

	/// `X-Real-IP` header (nginx).
	XRealIp,

	/// `CF-Connecting-IP` (Cloudflare / cloudflared).
	CfConnectingIp,

	/// `True-Client-IP` (Akamai, Cloudflare Enterprise).
	TrueClientIp,

	/// `Fly-Client-IP` (Fly.io).
	FlyClientIp,

	/// `CloudFront-Viewer-Address` (AWS CloudFront).
	#[serde(rename = "cloudfront_viewer_address")]
	CloudFrontViewerAddress,
}

/// The stand-in peer address the Unix listener reports for every client.
///
/// It is loopback so that socket clients take the loopback-peer rules: the
/// general client IP extractor bypasses a configured source for them, and
/// [`IpSource::identifies_client`] exempts them from the per-address password
/// limit.
pub const UNIX_SOCKET_PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

/// Whether an address taken from this source identifies a single client.
///
/// A loopback TCP peer may be a local reverse proxy, the Unix listener's
/// [`UNIX_SOCKET_PEER`] or a local client, so it can stand for many clients at
/// once. A forwarding header names the client itself, loopback or not.
#[implement(IpSource)]
#[inline]
#[must_use]
pub fn identifies_client(self, address: IpAddr) -> bool {
	self.is_header() || !address.to_canonical().is_loopback()
}

/// Whether this source reads a forwarding header rather than the TCP peer.
///
/// Every variant but `ConnectInfo` reads a header, which only a trusted reverse
/// proxy can vouch for.
#[implement(IpSource)]
#[inline]
#[must_use]
pub fn is_header(self) -> bool { !matches!(self, Self::ConnectInfo) }
