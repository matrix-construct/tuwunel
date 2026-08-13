use std::net::{SocketAddr, TcpListener};

use super::{bind_addrs, covers};

fn addr(addr: &str) -> SocketAddr { addr.parse().expect("test address parses") }

fn occupied_addr() -> (TcpListener, SocketAddr) {
	let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port binds");
	let addr = listener.local_addr().expect("bound address");

	(listener, addr)
}

#[test]
fn covers_the_same_address() {
	assert!(covers(&addr("127.0.0.1:8448"), &addr("127.0.0.1:8448")));
	assert!(covers(&addr("[::1]:8448"), &addr("[::1]:8448")));
}

#[test]
fn covers_its_family_from_the_wildcard() {
	assert!(covers(&addr("0.0.0.0:8448"), &addr("127.0.0.1:8448")));
	assert!(covers(&addr("[::]:8448"), &addr("[::1]:8448")));
}

#[test]
fn leaves_the_other_family_to_bind() {
	assert!(!covers(&addr("0.0.0.0:8448"), &addr("[::1]:8448")));
	assert!(!covers(&addr("[::]:8448"), &addr("127.0.0.1:8448")));
}

#[test]
fn leaves_another_port_to_bind() {
	assert!(!covers(&addr("0.0.0.0:8448"), &addr("127.0.0.1:8008")));
	assert!(!covers(&addr("127.0.0.1:8448"), &addr("127.0.0.1:8008")));
}

#[test]
fn a_specific_address_covers_no_wildcard() {
	assert!(!covers(&addr("127.0.0.1:8448"), &addr("0.0.0.0:8448")));
}

#[test]
fn skips_a_defaulted_address_it_cannot_bind() {
	let (_occupied, taken) = occupied_addr();
	let defaulted = true;
	let mut listening = Vec::new();
	let listeners =
		bind_addrs(&[taken], &mut listening, defaulted).expect("the address is skipped");

	assert!(listeners.is_empty(), "{listeners:?}");
	assert!(listening.is_empty(), "{listening:?}");
}

#[test]
fn fails_on_a_configured_address_it_cannot_bind() {
	let (_occupied, taken) = occupied_addr();
	let defaulted = false;
	let mut listening = Vec::new();

	bind_addrs(&[taken], &mut listening, defaulted).expect_err("the address fails loudly");
}

#[cfg(unix)]
#[test]
fn same_path_matches_one_socket() {
	use std::path::Path;

	use super::same_path;

	assert!(same_path(
		Path::new("/run/tuwunel/tuwunel.sock"),
		Path::new("/run/tuwunel/tuwunel.sock")
	));
	assert!(!same_path(Path::new("/run/tuwunel/tuwunel.sock"), Path::new("/run/other.sock")));
}
