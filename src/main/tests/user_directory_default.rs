#![cfg(test)]

mod user_directory;

use tuwunel_core::Result;

use self::user_directory::check_directory;

#[test]
fn appservice_accounts_remain_hidden_by_default() -> Result {
	check_directory(None, true, &["@directory_human:localhost"])
}
