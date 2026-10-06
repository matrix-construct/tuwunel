use ruma::{
	api::appservice::{Namespace, Namespaces, RegistrationInit},
	server_name, user_id,
};
use tuwunel_service::appservice::RegistrationInfo;

use super::assert_account_data_owner;

#[test]
fn appservice_account_data_access_is_limited_to_its_namespace() {
	let registration = RegistrationInit {
		id: "bridge".to_owned(),
		url: None,
		as_token: "as_token".to_owned(),
		hs_token: "hs_token".to_owned(),
		sender_localpart: "bridge_bot".to_owned(),
		namespaces: Namespaces {
			users: vec![Namespace::new(true, "^@bridged_.*$".to_owned())],
			..Default::default()
		},
		rate_limited: None,
		protocols: None,
	}
	.into();

	let appservice = RegistrationInfo::new(registration, server_name!("example.com"))
		.expect("valid registration");

	let sender = user_id!("@bridge_bot:example.com");
	let owner = |user_id| assert_account_data_owner(sender, user_id, Some(&appservice), "");

	owner(sender).expect("the appservice sender is in its namespace");
	owner(user_id!("@bridged_alice:example.com")).expect("a namespaced user is in its namespace");
	assert!(owner(user_id!("@alice:example.com")).is_err());
}
