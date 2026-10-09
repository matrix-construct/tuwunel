use std::{
	io::{Error, ErrorKind::PermissionDenied},
	iter::once,
	net::{IpAddr, SocketAddr},
	sync::Arc,
	time::SystemTime,
};

use hickory_resolver::proto::rr::{IntoName, Name as DnsName};
use ipaddress::IPAddress;
use minicbor_serde::{from_slice, to_vec};
use reqwest::{
	Url,
	dns::{Addrs, Name, Resolve, Resolving},
};
use ruma::OwnedServerName;
use tuwunel_core::{
	Result,
	config::{Figment, proxy::ProxyHosts},
};

use super::{
	cache::{CachedDest, CachedOverride, IpAddrs},
	dns::{Resolver, Validating},
	fed::{FedDest, add_port_to_hostname, get_ip_with_port, unbracket},
};
use crate::test_utils::fixture;

const SRV_TARGET: &str = "target.example";

const INVALID_SRV_TARGET: &str = "://target.example";

// A `CachedDest` row written before `srv`: destination and host `x:8448`, expiring
// at the epoch.
const LEGACY_DEST: &[u8] = b"\xa3\x64dest\xa1\x65Named\x82\x61x\x65:8448\x64host\x66x:8448\x66expire\xa2\x70secs_since_epoch\x00\x71nanos_since_epoch\x00";

#[derive(Debug)]
struct FixedResolver(SocketAddr);

#[derive(Debug)]
struct SequenceResolver(Vec<SocketAddr>);

impl Resolve for FixedResolver {
	fn resolve(&self, _name: Name) -> Resolving {
		let addr = self.0;
		let addrs: Addrs = Box::new(once(addr));

		Box::pin(async move { Ok(addrs) })
	}
}

impl Resolve for SequenceResolver {
	fn resolve(&self, _name: Name) -> Resolving {
		let addrs: Addrs = Box::new(self.0.clone().into_iter());

		// Resolve requires a boxed Resolving future.
		Box::pin(async move { Ok(addrs) })
	}
}

fn validating(addr: SocketAddr) -> Arc<Validating<FixedResolver>> {
	let inner = Arc::new(FixedResolver(addr));
	let denylist =
		Arc::from([IPAddress::parse("10.0.0.0/8").expect("test denylist range parses")]);

	let proxy_hosts: ProxyHosts = Arc::from(["proxy.internal".into()]);

	Validating::new(inner, denylist, proxy_hosts)
}

#[test]
fn ips_get_default_ports() {
	assert_eq!(
		get_ip_with_port("1.1.1.1"),
		Some(FedDest::Literal("1.1.1.1:8448".parse().unwrap()))
	);
	assert_eq!(
		get_ip_with_port("dead:beef::"),
		Some(FedDest::Literal("[dead:beef::]:8448".parse().unwrap()))
	);

	assert_eq!(
		get_ip_with_port("[dead:beef::]"),
		Some(FedDest::Literal("[dead:beef::]:8448".parse().unwrap()))
	);
}

#[test]
fn unbracket_requires_matching_outer_brackets() {
	assert_eq!(unbracket("[::1]"), "::1");
	assert_eq!(unbracket("[::1"), "[::1");
	assert_eq!(unbracket("::1]"), "::1]");
	assert_eq!(unbracket("[[::1]]"), "[::1]");
	assert!(get_ip_with_port("[::1").is_none());
	assert!(get_ip_with_port("::1]").is_none());
	assert!(get_ip_with_port("[[::1]]").is_none());
	assert!(get_ip_with_port("[::1]:65536").is_none());
	OwnedServerName::try_from("[::1").unwrap_err();
	OwnedServerName::try_from("[[::1]]").unwrap_err();
	OwnedServerName::try_from("[::1]:65536").unwrap_err();
	Url::parse("https://[[::1]]/").unwrap_err();
}

#[tokio::test]
async fn literal_routes_apply_default_policy_and_preserve_authority() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let denied = ["0.0.0.0", "[::]", "[::1]", "[::ffff:127.0.0.1]"];

	for name in denied {
		let name = OwnedServerName::try_from(name).expect("test server name parses");
		let error = fixture
			.services
			.resolver
			.get_actual_dest(&name)
			.await
			.expect_err("default-denied literal is rejected");

		assert!(
			error
				.to_string()
				.contains("Not allowed to send requests to this IP")
		);
	}

	for (name, socket, authority) in [
		("8.8.8.8", "8.8.8.8:8448", "8.8.8.8:8448"),
		(
			"[2001:4860:4860::8888]",
			"[2001:4860:4860::8888]:8448",
			"[2001:4860:4860::8888]:8448",
		),
		(
			"[2001:4860:4860::8888]:9448",
			"[2001:4860:4860::8888]:9448",
			"[2001:4860:4860::8888]:9448",
		),
		("[::ffff:8.8.8.8]", "[::ffff:8.8.8.8]:8448", "[::ffff:8.8.8.8]:8448"),
	] {
		let name = OwnedServerName::try_from(name).expect("test server name parses");
		let actual = fixture
			.services
			.resolver
			.get_actual_dest(&name)
			.await?;

		assert_eq!(actual.dest, FedDest::Literal(socket.parse().expect("test socket parses")));
		assert_eq!(actual.host.as_str(), authority);
	}

	Ok(())
}

#[tokio::test]
async fn empty_literal_denylist_allows_private_and_unspecified_routes() -> Result {
	let config = Figment::new().merge(("ip_range_denylist", Vec::<String>::new()));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	for name in ["0.0.0.0", "[::]", "[::1]", "[::ffff:127.0.0.1]"] {
		let name = OwnedServerName::try_from(name).expect("test server name parses");

		fixture
			.services
			.resolver
			.get_actual_dest(&name)
			.await?;
	}

	Ok(())
}

#[tokio::test]
async fn delegated_ipv6_routes_preserve_their_authority() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	for (delegated, socket, authority) in [
		("[::1]", "[::1]:8448", "[::1]:8448"),
		("[::1]:9448", "[::1]:9448", "[::1]:9448"),
		(
			"[2001:4860:4860:0:0:0:0:8888]",
			"[2001:4860:4860::8888]:8448",
			"[2001:4860:4860::8888]:8448",
		),
		("[::ffff:127.0.0.1]", "[::ffff:127.0.0.1]:8448", "[::ffff:127.0.0.1]:8448"),
	] {
		let actual = fixture
			.services
			.resolver
			.actual_dest_3(false, delegated)
			.await?;

		assert_eq!(actual.dest, FedDest::Literal(socket.parse().expect("test socket parses")));
		assert_eq!(actual.host.as_str(), authority);
		assert!(!actual.srv);
	}

	Ok(())
}

#[test]
fn ips_keep_custom_ports() {
	assert_eq!(
		get_ip_with_port("1.1.1.1:1234"),
		Some(FedDest::Literal("1.1.1.1:1234".parse().unwrap()))
	);
	assert_eq!(
		get_ip_with_port("[dead::beef]:8933"),
		Some(FedDest::Literal("[dead::beef]:8933".parse().unwrap()))
	);
}

#[test]
fn hostnames_get_default_ports() {
	assert_eq!(
		add_port_to_hostname("example.com"),
		FedDest::Named("example.com".into(), ":8448".try_into().unwrap())
	);
}

#[test]
fn hostnames_keep_custom_ports() {
	assert_eq!(
		add_port_to_hostname("example.com:1337"),
		FedDest::Named("example.com".into(), ":1337".try_into().unwrap())
	);
}

#[test]
fn eviction_key_matches_delegated_override_key() {
	// Overrides are keyed by the delegated host without a port; eviction derives
	// the same key from the resolved destination via `hostname()`, not origin.
	let delegated = add_port_to_hostname("delegated.example");
	let with_port = FedDest::Named("delegated.example".into(), ":8449".try_into().unwrap());

	assert_eq!(delegated.hostname().as_str(), "delegated.example");
	assert_eq!(with_port.hostname().as_str(), "delegated.example");
	assert_ne!(delegated.hostname().as_str(), "origin.example");
}

#[test]
fn srv_target_replaces_an_override_not_pointing_at_it() {
	assert!(!cached_override(None).covers(SRV_TARGET));
	assert!(!cached_override(Some("other.example")).covers(SRV_TARGET));
	assert!(cached_override(Some(SRV_TARGET)).covers(SRV_TARGET));
}

fn cached_override(overriding: Option<&str>) -> CachedOverride {
	CachedOverride {
		ips: IpAddrs::new(),
		port: 8448,
		expire: CachedOverride::default_expire(),
		overriding: overriding.map(Into::into),
	}
}

#[test]
fn expired_override_covers_nothing() {
	let expired = CachedOverride {
		expire: SystemTime::UNIX_EPOCH,
		..cached_override(Some(SRV_TARGET))
	};

	assert!(!expired.covers(SRV_TARGET));
}

#[test]
fn destinations_without_route_metadata_require_rediscovery() {
	let error = from_slice::<CachedDest>(LEGACY_DEST).unwrap_err();

	assert!(error.to_string().contains("srv"), "{error}");
}

#[test]
fn destination_route_metadata_roundtrips() {
	for srv in [false, true] {
		let bytes = destination_bytes(srv);
		let cached = from_slice::<CachedDest>(&bytes).unwrap();

		assert_eq!(cached.dest, add_port_to_hostname("x"));
		assert_eq!(cached.host.as_str(), "x:8448");
		assert_eq!(cached.expire, SystemTime::UNIX_EPOCH);
		assert_eq!(cached.srv, srv);

		let encoded = to_vec(&cached).unwrap();
		let decoded = from_slice::<CachedDest>(&encoded).unwrap();

		assert_eq!(decoded.dest, cached.dest);
		assert_eq!(decoded.host, cached.host);
		assert_eq!(decoded.expire, cached.expire);
		assert_eq!(decoded.srv, srv);
	}
}

fn destination_bytes(srv: bool) -> Vec<u8> {
	let flag = if srv { 0xF5 } else { 0xF4 };

	[&[0xA4][..], &LEGACY_DEST[1..], b"\x63srv", &[flag]].concat()
}

#[test]
fn nameservers_get_default_ports() {
	let conf = Resolver::parse_nameserver("1.1.1.1").unwrap();

	assert_eq!(conf.ip, "1.1.1.1".parse::<IpAddr>().unwrap());
	assert!(!conf.connections.is_empty());
	assert!(
		conf.connections
			.iter()
			.all(|conn| conn.port == 53)
	);
}

#[test]
fn nameservers_keep_custom_ports() {
	let conf = Resolver::parse_nameserver("127.0.0.1:5353").unwrap();

	assert_eq!(conf.ip, "127.0.0.1".parse::<IpAddr>().unwrap());
	assert!(!conf.connections.is_empty());
	assert!(
		conf.connections
			.iter()
			.all(|conn| conn.port == 5353)
	);

	let conf = Resolver::parse_nameserver("[dead::beef]:5353").unwrap();

	assert_eq!(conf.ip, "dead::beef".parse::<IpAddr>().unwrap());
	assert!(!conf.connections.is_empty());
	assert!(
		conf.connections
			.iter()
			.all(|conn| conn.port == 5353)
	);
}

#[test]
fn nameservers_reject_hostnames() {
	Resolver::parse_nameserver("dns.example.com").unwrap_err();
	Resolver::parse_nameserver("").unwrap_err();
}

#[test]
fn lookups_parse_hosts_like_the_invalid_name_guard() {
	// `handle_resolve_error` demotes a lookup error only when `DnsName::from_utf8` rejects the host.
	let hosts = [INVALID_SRV_TARGET, "[2001", SRV_TARGET, "_matrix-fed._tcp.example", "1.1.1.1"];

	for host in hosts {
		let invalid = DnsName::from_utf8(host).is_err();

		assert_eq!(host.into_name().is_err(), invalid, "{host}");
		assert_eq!(host.to_owned().into_name().is_err(), invalid, "{host}");
	}

	DnsName::from_utf8(INVALID_SRV_TARGET).unwrap_err();
	DnsName::from_utf8(SRV_TARGET).unwrap();
}

#[tokio::test]
async fn validating_resolver_allows_a_denied_proxy_host() {
	let addr = "10.1.2.3:1080"
		.parse()
		.expect("test address parses");

	let resolver = validating(addr);
	let name = "PrOxY.InTeRnAl"
		.parse()
		.expect("test hostname parses");

	let mut resolved = resolver
		.resolve(name)
		.await
		.expect("proxy host bypasses the destination denylist");

	assert_eq!(resolved.next(), Some(addr));
	assert_eq!(resolved.next(), None);
}

#[tokio::test]
async fn validating_resolver_still_denies_a_destination_host() {
	let addr = "10.1.2.3:443"
		.parse()
		.expect("test address parses");

	let resolver = validating(addr);
	let name = "destination.internal"
		.parse()
		.expect("test hostname parses");

	let Err(error) = resolver.resolve(name).await else {
		panic!("destination host unexpectedly bypassed the denylist");
	};

	let error = error
		.downcast_ref::<Error>()
		.expect("denylist failure is an IO error");

	assert_eq!(error.kind(), PermissionDenied);
	assert_eq!(error.to_string(), "All resolved addresses are denied by ip_range_denylist");
}

#[tokio::test]
async fn validating_resolver_filters_mapped_and_unspecified_addresses_in_order() {
	let denied_mapped = "[::ffff:10.1.2.3]:443"
		.parse()
		.expect("test address parses");

	let denied_unspecified = "[::]:443".parse().expect("test address parses");
	let allowed_ipv6 = "[2001:4860:4860::8888]:443"
		.parse()
		.expect("test address parses");

	let allowed_ipv4 = "8.8.8.8:443"
		.parse()
		.expect("test address parses");

	let inner = Arc::new(SequenceResolver(vec![
		denied_mapped,
		allowed_ipv6,
		denied_unspecified,
		allowed_ipv4,
	]));

	let denylist = Arc::from([
		IPAddress::parse("10.0.0.0/8").expect("test denylist range parses"),
		IPAddress::parse("::/128").expect("test denylist range parses"),
	]);

	let resolver = Validating::new(inner, denylist, Arc::from([]));
	let name = "destination.example"
		.parse()
		.expect("test hostname parses");

	let resolved = resolver
		.resolve(name)
		.await
		.expect("allowed addresses remain");

	assert_eq!(resolved.collect::<Vec<_>>(), [allowed_ipv6, allowed_ipv4]);
}
