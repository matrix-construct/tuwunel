#![cfg(test)]

use fixture::run as run_fixture;
use tuwunel_core::{
	Result,
	ruma::exports::serde::{Deserialize, Serialize},
};

#[path = "email_password_reset/fixture.rs"]
mod fixture;
#[path = "email_password_reset/http.rs"]
mod http;
#[path = "email_password_reset/scenarios.rs"]
mod scenarios;

#[derive(Deserialize, Serialize)]
#[serde(crate = "tuwunel_core::ruma::exports::serde")]
struct RestartState {
	sid: String,
	b_session: String,
	b_device: String,
	a_password_hash: String,
	b_password_hash: String,
	masked: (u16, String),
	deactivated_sid: String,
}

#[derive(Clone, Copy)]
struct Substitution<'a> {
	session: &'a str,
	user: &'a str,
	email: &'a str,
	device: &'a str,
}

struct Reset<'a> {
	sid: &'a str,
	client_secret: &'a str,
	new_password: &'a str,
	logout_devices: Option<bool>,
	substitution: Substitution<'a>,
}

#[test]
fn logged_out_email_password_reset_is_single_use() -> Result { run_fixture() }
