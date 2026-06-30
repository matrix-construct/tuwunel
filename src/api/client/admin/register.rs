use std::result::Result as StdResult;

use axum::extract::State;
use hmac::{Hmac, Mac};
use ruma::{OwnedUserId, UserId};
use sha1::Sha1;
use synapse_admin_api::register_users::shared_secret_register as register;
use tuwunel_core::{Err, Result, err};
use tuwunel_service::users::{Register, device::generate_refresh_token};

use crate::{ClientIp, Ruma};

type HmacSha1 = Hmac<Sha1>;

/// # `POST /_synapse/admin/v1/register`
///
/// Out-of-band account creation authenticated by HMAC over the homeserver's
/// registration shared secret. Bypasses UIAA. Mirrors Synapse's endpoint of
/// the same name.
pub(crate) async fn admin_register_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<register::v1::Request>,
) -> Result<register::v1::Response> {
	let Some(shared_secret) = services.admin.register_shared_secret() else {
		return Err!(Request(Unknown("Shared-secret registration is not enabled")));
	};

	if !services.admin.consume_register_nonce(&body.nonce) {
		return Err!(Request(InvalidParam("Unrecognised or expired nonce")));
	}

	check_field("Username", &body.username)?;
	check_field("Password", &body.password)?;

	verify_mac(shared_secret, &body)
		.map_err(|()| err!(Request(Forbidden("HMAC check failed"))))?;

	let user_id = resolve_local_user_id(services, &body.username)?;

	if services.users.exists(&user_id).await {
		return Err!(Request(UserInUse("User ID is not available")));
	}

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some(&body.password),
			displayname: body.displayname.as_deref(),
			grant_first_user_admin: false,
			..Default::default()
		})
		.await?;

	if body.admin {
		services.admin.make_user_admin(&user_id).await?;
	}

	let home_server = services.globals.server_name().to_owned();

	if body.inhibit_login {
		return Ok(register::v1::Response::new(user_id, home_server));
	}

	let (access_token, expires_in) = services
		.users
		.generate_access_token(body.refresh_token);

	let refresh_token = expires_in.is_some().then(generate_refresh_token);

	let device_id = services
		.users
		.create_device(
			&user_id,
			body.device_id.as_deref(),
			(Some(&access_token), expires_in),
			refresh_token.as_deref(),
			body.initial_device_display_name.as_deref(),
			Some(client),
		)
		.await?;

	Ok(register::v1::Response {
		user_id,
		home_server,
		access_token: Some(access_token),
		device_id: Some(device_id),
		refresh_token,
		expires_in,
	})
}

fn check_field(label: &str, value: &str) -> Result<()> {
	if value.is_empty() {
		return Err!(Request(InvalidParam("{label} must not be empty")));
	}

	if value.len() > 512 {
		return Err!(Request(InvalidParam("{label} must not exceed 512 bytes")));
	}

	if value.as_bytes().contains(&0) {
		return Err!(Request(InvalidParam("{label} must not contain a null byte")));
	}

	Ok(())
}

fn verify_mac(secret: &str, req: &register::v1::Request) -> StdResult<(), ()> {
	let admin = if req.admin {
		b"admin".as_slice()
	} else {
		b"notadmin".as_slice()
	};

	let parts = [req.nonce.as_bytes(), req.username.as_bytes(), req.password.as_bytes(), admin]
		.into_iter()
		.chain(req.user_type.as_deref().map(str::as_bytes));

	let mut mac = HmacSha1::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
	for (i, part) in parts.enumerate() {
		if i > 0 {
			mac.update(b"\0");
		}
		mac.update(part);
	}

	let expected = decode_hex(&req.mac).ok_or(())?;

	mac.verify_slice(&expected).map_err(|_| ())
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
	s.len().is_multiple_of(2).then_some(())?;

	s.as_bytes()
		.as_chunks::<2>()
		.0
		.iter()
		.map(|c| Some((hex_nibble(c[0])? << 4) | hex_nibble(c[1])?))
		.collect()
}

const fn hex_nibble(b: u8) -> Option<u8> {
	match b {
		| b'0'..=b'9' => Some(b.wrapping_sub(b'0')),
		| b'a'..=b'f' => Some(b.wrapping_sub(b'a').wrapping_add(10)),
		| b'A'..=b'F' => Some(b.wrapping_sub(b'A').wrapping_add(10)),
		| _ => None,
	}
}

fn resolve_local_user_id(services: crate::State, username: &str) -> Result<OwnedUserId> {
	let server_name = services.globals.server_name();
	let username = username.to_lowercase();

	let user_id = match username.starts_with('@') {
		| true => UserId::parse(&username),
		| false => UserId::parse_with_server_name(&username, server_name),
	}
	.map_err(|_| err!(Request(InvalidParam("Invalid user id"))))?;

	user_id.validate_strict().map_err(|_| {
		err!(Request(InvalidUsername("Username contains disallowed characters or spaces")))
	})?;

	if user_id.server_name() != server_name {
		return Err!(Request(InvalidParam("User is not local to this server")));
	}

	Ok(user_id)
}

#[cfg(test)]
mod tests {
	use super::{decode_hex, register, verify_mac};

	const SECRET: &str = "shared-secret-12345";
	const NONCE: &str = "thenonce0123456789abcdef";
	const USER: &str = "alice";
	const PASS: &str = "p4ssw0rd!";

	// Reference vectors from Python's hmac.new(SECRET, digestmod=sha1) over
	// NONCE\0USER\0PASS\0(admin|notadmin)[\0user_type].
	const MAC_NOTADMIN: &str = "44e0ec50d52aaa4029731dfcfe7e22123fa4c53e";
	const MAC_ADMIN: &str = "7774d6962a728b48ca7cd41e99a5149a93ff1ec5";
	const MAC_ADMIN_BOT: &str = "10f12d86c7210410ee777edadf655033b8f36008";

	fn req(admin: bool, user_type: Option<&str>, mac: &str) -> register::v1::Request {
		register::v1::Request {
			nonce: NONCE.into(),
			username: USER.into(),
			displayname: None,
			password: PASS.into(),
			admin,
			user_type: user_type.map(Into::into),
			mac: mac.into(),
			inhibit_login: false,
			refresh_token: false,
			device_id: None,
			initial_device_display_name: None,
		}
	}

	#[test]
	fn verify_mac_accepts_notadmin_reference_vector() {
		assert!(verify_mac(SECRET, &req(false, None, MAC_NOTADMIN)).is_ok());
	}

	#[test]
	fn verify_mac_accepts_admin_reference_vector() {
		assert!(verify_mac(SECRET, &req(true, None, MAC_ADMIN)).is_ok());
	}

	#[test]
	fn verify_mac_accepts_admin_with_user_type() {
		assert!(verify_mac(SECRET, &req(true, Some("bot"), MAC_ADMIN_BOT)).is_ok());
	}

	#[test]
	fn verify_mac_admin_flag_is_part_of_the_mac() {
		// the notadmin MAC must not validate when admin=true and vice versa.
		assert!(verify_mac(SECRET, &req(true, None, MAC_NOTADMIN)).is_err());
		assert!(verify_mac(SECRET, &req(false, None, MAC_ADMIN)).is_err());
	}

	#[test]
	fn verify_mac_user_type_is_part_of_the_mac() {
		// omitting user_type from the request must not validate the with-user_type MAC.
		assert!(verify_mac(SECRET, &req(true, None, MAC_ADMIN_BOT)).is_err());
	}

	#[test]
	fn verify_mac_rejects_wrong_secret() {
		assert!(verify_mac("other-secret", &req(false, None, MAC_NOTADMIN)).is_err());
	}

	#[test]
	fn verify_mac_rejects_malformed_hex() {
		assert!(verify_mac(SECRET, &req(false, None, "not-hex")).is_err());
		assert!(verify_mac(SECRET, &req(false, None, "abc")).is_err()); // odd length
	}

	#[test]
	fn verify_mac_accepts_uppercase_hex() {
		assert!(verify_mac(SECRET, &req(false, None, &MAC_NOTADMIN.to_uppercase())).is_ok());
	}

	#[test]
	fn decode_hex_roundtrips() {
		assert_eq!(decode_hex("00ff10ab"), Some(vec![0x00, 0xFF, 0x10, 0xAB]));
		assert_eq!(decode_hex(""), Some(vec![]));
		assert_eq!(decode_hex("0"), None);
		assert_eq!(decode_hex("0g"), None);
	}
}
