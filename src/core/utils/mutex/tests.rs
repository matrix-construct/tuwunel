use std::{sync::Mutex, thread::scope};

use super::MutexExt;

#[test]
fn lock_adopting_recovers_poison() {
	let mutex = Mutex::new(vec![1_u8]);

	scope(|threads| {
		let poisoner = threads.spawn(|| {
			let _held = mutex.lock_adopting();

			panic!("poisoning the lock");
		});

		assert!(poisoner.join().is_err(), "the poisoning thread did not panic");
	});

	assert!(mutex.is_poisoned(), "the lock was not poisoned");

	mutex.lock_adopting().push(2);

	assert_eq!(*mutex.lock_adopting(), [1, 2], "the adopted lock lost the write");
}
