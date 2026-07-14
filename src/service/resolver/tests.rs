use std::net::IpAddr;

use super::{
	dns::Resolver,
	fed::{FedDest, add_port_to_hostname, get_ip_with_port},
};

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
