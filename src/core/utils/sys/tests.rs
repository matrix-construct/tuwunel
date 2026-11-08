#![cfg(test)]

#[cfg(all(not(feature = "cpu_affinity"), not(target_os = "openbsd")))]
use std::thread::available_parallelism as std_parallelism;
use std::{env::current_dir, iter::once, thread::spawn};

#[cfg(all(feature = "cpu_affinity", target_os = "linux"))]
use core_affinity::{CoreId, get_core_ids};

#[cfg(all(not(feature = "cpu_affinity"), not(target_os = "openbsd")))]
use super::compute::available_parallelism;
use super::{
	compute::{cores_available, get_affinity, set_affinity},
	storage::available_space,
};

#[test]
fn available_space_reads_a_directory_and_fails_on_a_missing_one() {
	let here = current_dir().expect("current directory");
	let missing = here.join("no-such-directory-for-available-space");

	assert!(available_space(&here).expect("available space of the current directory") > 0);
	available_space(&missing).unwrap_err();
}

#[test]
fn set_affinity_records_mask() {
	on_last_core(|id| assert_eq!(get_affinity().collect::<Vec<_>>(), [id]));
}

// A fresh thread starts with no recorded affinity, and its pin stays off the harness thread.
fn on_last_core<F>(check: F)
where
	F: FnOnce(usize) + Send + 'static,
{
	spawn(move || {
		let id = cores_available()
			.last()
			.expect("an available core");

		set_affinity(once(id));
		check(id);
	})
	.join()
	.expect("affinity thread");
}

#[test]
#[cfg(all(not(feature = "cpu_affinity"), not(target_os = "openbsd")))]
fn cores_available_counts_std_parallelism() {
	let n = std_parallelism().map_or(1, usize::from);

	assert_eq!(available_parallelism(), n);
	assert_eq!(cores_available().collect::<Vec<_>>(), (0..n).collect::<Vec<_>>());
}

#[test]
#[cfg(all(feature = "cpu_affinity", target_os = "linux"))]
fn set_affinity_pins_thread() {
	on_last_core(|id| assert_eq!(get_core_ids(), Some(vec![CoreId { id }])));
}
