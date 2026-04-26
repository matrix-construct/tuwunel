#![cfg(test)]

use tuwunel_api::router::ConfiguredIpSource;
use tuwunel_core::config::IpSource;

use super::{configured_ip_source, ip_source_layer};

#[test]
fn configured_ip_source_maps_all_variants() {
	let cases = [
		(IpSource::ConnectInfo, "ConnectInfo"),
		(IpSource::RightmostXForwardedFor, "RightmostXForwardedFor"),
		(IpSource::RightmostForwarded, "RightmostForwarded"),
		(IpSource::XRealIp, "XRealIp"),
		(IpSource::CfConnectingIp, "CfConnectingIp"),
		(IpSource::TrueClientIp, "TrueClientIp"),
		(IpSource::FlyClientIp, "FlyClientIp"),
		(IpSource::CloudFrontViewerAddress, "CloudFrontViewerAddress"),
	];

	for (source, expected) in cases {
		let ConfiguredIpSource(actual) = ConfiguredIpSource(configured_ip_source(source));
		assert_eq!(format!("{actual:?}"), expected);
	}
}

#[test]
fn ip_source_layer_none_returns_identity_branch() {
	let layer = ip_source_layer(None);

	assert!(matches!(layer, tower::util::Either::Right(_)));
}

#[test]
fn ip_source_layer_connect_info_returns_extension_branch() {
	let layer = ip_source_layer(Some(IpSource::ConnectInfo));

	assert!(matches!(
		layer,
		tower::util::Either::Left(axum::Extension(ConfiguredIpSource(_)))
	));
}
