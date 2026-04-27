use serde::Deserialize;

/// Selects the source used to determine the connecting client's IP
/// address. Variants correspond 1:1 to `axum_client_ip::SecureClientIpSource`.
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
