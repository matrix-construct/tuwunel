use std::{
	io::{Error, ErrorKind::PermissionDenied},
	iter::once,
	net::{IpAddr, SocketAddr},
	sync::Arc,
	time::SystemTime,
};

use ipaddress::IPAddress;
use minicbor_serde::{from_slice, to_vec};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tuwunel_core::config::proxy::ProxyHosts;

use super::{
	cache::{CachedDest, CachedOverride, IpAddrs},
	dns::{Resolver, Validating},
	fed::{FedDest, add_port_to_hostname, get_ip_with_port},
};

const SRV_TARGET: &str = "target.example";

// A `CachedDest` row written before `srv`: destination and host `x:8448`, expiring
// at the epoch.
const LEGACY_DEST: &[u8] = b"\xa3\x64dest\xa1\x65Named\x82\x61x\x65:8448\x64host\x66x:8448\x66expire\xa2\x70secs_since_epoch\x00\x71nanos_since_epoch\x00";

#[derive(Debug)]
struct FixedResolver(SocketAddr);

impl Resolve for FixedResolver {
	fn resolve(&self, _name: Name) -> Resolving {
		let addr = self.0;
		let addrs: Addrs = Box::new(once(addr));

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
