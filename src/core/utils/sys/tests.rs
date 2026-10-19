#![cfg(test)]

use std::env::current_dir;

use super::storage::available_space;

#[test]
fn available_space_reads_a_directory_and_fails_on_a_missing_one() {
	let here = current_dir().expect("current directory");
	let missing = here.join("no-such-directory-for-available-space");

	assert!(available_space(&here).expect("available space of the current directory") > 0);
	available_space(&missing).unwrap_err();
}
