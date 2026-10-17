#![cfg(test)]

use tuwunel_core::{
	Err, Result,
	ruma::{UserId, device_id},
};
use tuwunel_service::{Services, users::device::generate_refresh_token};

use self::fixture::boot;

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

mod fixture;

#[test]
fn refresh_device_lock() -> Result {
	let options = ["refresh_token_reuse_grace=3600"];

	boot("refresh-device-lock", options, exercise)
}

async fn exercise(services: &Services, _base: &str) -> Result {
	removed_device_is_issued_no_tokens(services).await?;
	concurrent_refreshes_keep_one_successor(services).await
}

/// A refresh still in flight when its device is removed must not leave tokens
/// behind for the removed device.
async fn removed_device_is_issued_no_tokens(services: &Services) -> Result {
	let user = UserId::parse_with_server_name("removed", services.globals.server_name())?;
	let device = device_id!("REMOVEDDEVICE");

	services.users.create(&user, None, None).await?;
	services
		.users
		.create_device(&user, Some(device), (None, None), None, None, None)
		.await?;

	services.users.remove_device(&user, device).await;

	let (access, expires_in) = services.users.generate_access_token(true);
	let refresh = generate_refresh_token();
	let issued = services
		.users
		.set_access_token(&user, device, &access, expires_in, Some(&refresh))
		.await;

	if issued.is_ok()
		|| services
			.users
			.find_from_token(&refresh)
			.await
			.is_ok()
	{
		return Err!("tokens were issued to a removed device");
	}

	Ok(())
}

/// Two refreshes that both found the same refresh token current rotate it
/// once: the later one keeps the earlier one's successor.
async fn concurrent_refreshes_keep_one_successor(services: &Services) -> Result {
	let user = UserId::parse_with_server_name("concurrent", services.globals.server_name())?;
	let device = device_id!("CONCURRENTDEVICE");
	let original = generate_refresh_token();
	let (access, expires_in) = services.users.generate_access_token(true);

	services.users.create(&user, None, None).await?;
	services
		.users
		.create_device(
			&user,
			Some(device),
			(Some(&access), expires_in),
			Some(&original),
			None,
			None,
		)
		.await?;

	let mut successors = Vec::new();
	for _ in 0..2 {
		let (access, expires_in) = services.users.generate_access_token(true);
		let successor = services
			.users
			.rotate_refresh_token(&user, device, &original, &access, expires_in)
			.await?;

		successors.push(successor);
	}

	if successors[0] != successors[1] {
		return Err!("the refresh token was rotated twice");
	}

	Ok(())
}
