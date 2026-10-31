#![cfg(test)]

use tuwunel_core::{Result, result::NotFound};
use tuwunel_service::Services;

use self::{admin::accepted, client::register, fixture::boot};

#[expect(dead_code)] // Every command here is expected to succeed.
mod admin;
#[expect(dead_code)] // Only registration is used from the client harness.
mod client;
mod fixture;

const RESET_TOKEN: &str = "admin-reset-password-reset-access-token";
const KEPT_TOKEN: &str = "admin-reset-password-kept-access-token";

/// `reset-password` logs out every device of the user unless told to keep
/// them.
///
/// An access token issued before the reset no longer authenticates once the
/// command has run, while a reset passing `--no-logout-devices` leaves the
/// user's session signed in.
#[test]
fn logs_out_devices_unless_kept() -> Result {
	boot("admin-reset-password", ["admin.room.create=true"], exercise)
}

async fn exercise(services: &Services, _: &str) -> Result {
	let reset = register(services, "resetuser", RESET_TOKEN).await?;
	let kept = register(services, "keptuser", KEPT_TOKEN).await?;

	assert!(has_session(services, RESET_TOKEN).await?);

	let reset_command = format!("users reset-password {reset} new-password");
	let kept_command = format!("users reset-password --no-logout-devices {kept} new-password");

	accepted(services, &reset_command).await?;
	accepted(services, &kept_command).await?;

	assert!(!has_session(services, RESET_TOKEN).await?);
	assert!(has_session(services, KEPT_TOKEN).await?);

	Ok(())
}

async fn has_session(services: &Services, token: &str) -> Result<bool> {
	services
		.users
		.find_from_token(token)
		.await
		.optional()
		.map(|session| session.is_some())
}
