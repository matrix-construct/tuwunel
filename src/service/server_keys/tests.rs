use ruma::{ServerSigningKeyId, server_name};
use tuwunel_core::{Result, config::Figment};

use crate::test_utils::fixture;

/// Only this server's active key is held without a stored copy.
///
/// Another key ID of ours, and our active key ID under another server's name,
/// both still need one.
#[tokio::test]
async fn only_the_active_local_key_is_held_without_a_stored_copy() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let server_keys = &fixture.services.server_keys;
	let held = |server, key_id| server_keys.verify_key_exists(server, key_id);
	let ours = fixture.services.globals.server_name();
	let active = server_keys.active_key_id();
	let retired = <&ServerSigningKeyId>::try_from("ed25519:retired")?;
	let remote = server_name!("remote.test");

	assert!(held(ours, active).await, "our active key");
	assert!(!held(ours, retired).await, "our retired key");
	assert!(!held(remote, active).await, "a remote's key");

	Ok(())
}
