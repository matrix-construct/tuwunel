use super::{
	actual::srv_override_plan,
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
fn srv_override_plan_uses_srv_target_as_url_hostname() {
	let srv = FedDest::Named("matrix.example.net".into(), ":443".try_into().unwrap());

	let plan = srv_override_plan("example.com", &srv);

	assert_eq!(plan.base_hostname.as_str(), "example.com");
	assert_eq!(plan.srv_hostname.as_str(), "matrix.example.net");
	assert_eq!(plan.srv_port, 443);
	assert_eq!(
		plan.url_dest,
		FedDest::Named("matrix.example.net".into(), ":443".try_into().unwrap()),
	);
}
