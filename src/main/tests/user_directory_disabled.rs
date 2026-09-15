#![cfg(test)]

mod user_directory;

use tuwunel_core::Result;

use self::user_directory::check_directory;

#[test]
fn appservice_visibility_can_be_disabled_explicitly() -> Result {
	check_directory(Some(false), true, &["@directory_human:localhost"])
}
