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

/// A refresh still in flight when its device is removed must not leave tokens
/// behind for the removed device.
#[test]
fn removed_device_is_issued_no_tokens() -> Result {
	let options: [&str; 0] = [];

	boot("refresh-removed-device", options, exercise)
}

async fn exercise(services: &Services, _base: &str) -> Result {
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
