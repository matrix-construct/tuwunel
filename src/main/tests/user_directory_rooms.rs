#![cfg(test)]

mod user_directory;

use tuwunel_core::Result;

use self::user_directory::check_directory;

#[test]
fn appservice_visibility_preserves_room_visibility_rules() -> Result {
	check_directory(Some(true), false, &[])
}
