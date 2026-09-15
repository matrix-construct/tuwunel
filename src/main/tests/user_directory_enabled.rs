#![cfg(test)]

mod user_directory;

use tuwunel_core::Result;

use self::user_directory::check_directory;

#[test]
fn appservice_accounts_can_be_made_discoverable() -> Result {
	check_directory(Some(true), true, &[
		"@directory_agent:localhost",
		"@directory_human:localhost",
		"@directory_sender:localhost",
	])
}
