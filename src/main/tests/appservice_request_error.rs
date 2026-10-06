#![cfg(test)]

use std::net::TcpListener;

use tuwunel_core::{
	Result,
	ruma::api::appservice::{
		Namespaces, Registration, RegistrationInit, ping::send_ping, thirdparty::get_protocol,
	},
};
use tuwunel_service::Services;

use self::fixture::boot;

#[expect(dead_code)] // Only the readiness probe is used from the client harness.
mod client;
mod fixture;

const HS_TOKEN: &str = "appservice-request-error-hs-token";

/// Requests to an unreachable appservice fail, and are logged, without the
/// `hs_token` that their URL carries as the `access_token` query.
#[test]
fn unreachable_appservice_errors_omit_hs_token() -> Result {
	boot("appservice-request-error", None::<&str>, exercise)
}

async fn exercise(services: &Services, _: &str) -> Result {
	// Nothing listens on the port once the listener closes.
	let closed = TcpListener::bind(("127.0.0.1", 0))?.local_addr()?;

	let registration: Registration = RegistrationInit {
		id: "unreachable".to_owned(),
		url: Some(format!("http://{closed}")),
		as_token: "appservice-request-error-as-token".to_owned(),
		hs_token: HS_TOKEN.to_owned(),
		sender_localpart: "unreachable".to_owned(),
		namespaces: Namespaces::new(),
		rate_limited: None,
		protocols: None,
	}
	.into();

	let sent = services
		.appservice
		.send_request(registration.clone(), get_protocol::v1::Request::new("irc".to_owned()))
		.await;

	let pinged = services
		.appservice
		.ping(registration, send_ping::v1::Request::new())
		.await;

	assert!(sent.is_err_and(|e| !format!("{e:?}").contains(HS_TOKEN)));
	assert!(pinged.is_err_and(|e| !format!("{e:?}").contains(HS_TOKEN)));

	Ok(())
}
