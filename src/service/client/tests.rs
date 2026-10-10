use ipaddress::IPAddress;
use reqwest::Url;
use tuwunel_core::{Result, config::Figment};

use super::valid_cidr_range_url;
use crate::test_utils::fixture;

#[test]
fn url_cidr_check_handles_ipv4_ipv6_and_domains() {
	let denylist = [
		IPAddress::parse("10.0.0.0/8").expect("test denylist range parses"),
		IPAddress::parse("::1/128").expect("test denylist range parses"),
	];

	let ipv4 = Url::parse("http://10.1.2.3/").expect("test URL parses");
	let ipv6 = Url::parse("http://[::1]/").expect("test URL parses");
	let allowed = Url::parse("https://8.8.8.8/").expect("test URL parses");
	let domain = Url::parse("https://example.com/").expect("test URL parses");

	assert!(!valid_cidr_range_url(&denylist, &ipv4));
	assert!(!valid_cidr_range_url(&denylist, &ipv6));
	assert!(valid_cidr_range_url(&denylist, &allowed));
	assert!(valid_cidr_range_url(&denylist, &domain));
}

#[test]
fn url_cidr_check_matches_ipv4_mapped_ipv6_in_both_forms() {
	let ipv4 = [IPAddress::parse("10.0.0.0/8").expect("test denylist range parses")];
	let mapped = [IPAddress::parse("::ffff:0:0/96").expect("test denylist range parses")];

	let private = Url::parse("http://[::ffff:10.1.2.3]/").expect("test URL parses");
	let public = Url::parse("http://[::ffff:8.8.8.8]/").expect("test URL parses");

	assert!(!valid_cidr_range_url(&ipv4, &private));
	assert!(valid_cidr_range_url(&ipv4, &public));
	assert!(!valid_cidr_range_url(&mapped, &public));
}

/// The discovery client refuses a plain HTTP URL without connecting to it.
///
/// reqwest applies the same `https_only` check to every redirect hop, so a
/// peer's redirect cannot move the lookup to plain HTTP either.
#[tokio::test]
async fn well_known_refuses_plain_http_before_connecting() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let well_known = &fixture.services.client.well_known;
	let error = well_known
		.get("http://127.0.0.1:0/")
		.send()
		.await
		.expect_err("plain HTTP is refused");

	assert!(error.is_builder(), "refused as a bad scheme, not a connect failure: {error}");

	Ok(())
}
