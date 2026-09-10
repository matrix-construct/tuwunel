#![cfg(test)]

use axum::Extension;
use http::{
	HeaderMap, Request, Response,
	header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_FRAME_OPTIONS},
};
use ipnet::IpNet;
use tower::util::Either;
use tuwunel_api::router::{ConfiguredIpSource, TrustedPeerSubnets};
use tuwunel_core::config::IpSource;

use super::{ip_source_layer, set_html_headers, trusted_peer_subnets_layer};

#[test]
fn ip_source_layer_none_returns_identity_branch() {
	let layer = ip_source_layer(None);

	assert!(matches!(layer, Either::Right(_)));
}

#[test]
fn ip_source_layer_connect_info_returns_extension_branch() {
	let layer = ip_source_layer(Some(IpSource::ConnectInfo));

	assert!(matches!(layer, Either::Left(Extension(ConfiguredIpSource(_)))));
}

#[test]
fn trusted_peer_subnets_layer_empty_returns_identity_branch() {
	let layer = trusted_peer_subnets_layer(&[]);

	assert!(matches!(layer, Either::Right(_)));
}

#[test]
fn trusted_peer_subnets_layer_populated_returns_extension_branch() {
	let subnets: Vec<IpNet> =
		vec!["172.18.0.0/16".parse().expect("CIDR"), "fd00::/8".parse().expect("CIDR")];

	let layer = trusted_peer_subnets_layer(&subnets);

	let nets = match layer {
		| Either::Left(Extension(TrustedPeerSubnets(nets))) => nets,
		| Either::Right(_) => panic!("expected extension branch"),
	};

	assert_eq!(nets.len(), 2);
}

#[test]
fn request_path_str_drops_query_string() {
	let request = Request::builder()
		.uri("/_matrix/client/v3/sync?access_token=syt_realtoken")
		.body(())
		.expect("valid request");

	let path = super::request_path_str(&request);

	assert_eq!(path, "/_matrix/client/v3/sync");
	assert!(!path.contains('?'));
	assert!(!path.contains("access_token"));
}

#[test]
fn html_is_framed_and_constrained_in_any_case() {
	for content_type in ["text/html", "Text/HTML", "text/html; charset=utf-8"] {
		let headers = html_headers_for(content_type);

		assert!(
			headers.contains_key(CONTENT_SECURITY_POLICY),
			"{content_type} gets a content security policy"
		);
		assert!(headers.contains_key(X_FRAME_OPTIONS), "{content_type} is denied framing");
	}
}

#[test]
fn a_parameter_mentioning_html_is_left_alone() {
	for content_type in ["application/json; x=text/html", "application/json"] {
		let headers = html_headers_for(content_type);

		assert!(!headers.contains_key(CONTENT_SECURITY_POLICY), "{content_type} is not html");
		assert!(!headers.contains_key(X_FRAME_OPTIONS), "{content_type} is not html");
	}
}

fn html_headers_for(content_type: &str) -> HeaderMap {
	let response = Response::builder()
		.header(CONTENT_TYPE, content_type)
		.body(())
		.expect("the response builds");

	set_html_headers(response).headers().clone()
}
