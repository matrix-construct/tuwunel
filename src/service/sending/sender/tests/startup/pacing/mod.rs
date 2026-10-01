use std::{cmp::Reverse, time::Duration};

use tokio::time::Instant;
use tuwunel_core::Result;

use super::{config, deadline, destination, fixture};
use crate::{
	federation::{Classification, ShouldAttempt},
	sending::{Destination, sender::WakeQueue},
};

#[tokio::test]
async fn paced_boot_wakes_respect_each_arming_time() -> Result { pacing(20).await }

#[tokio::test]
async fn zero_timeout_still_paces_boot_wakes() -> Result { pacing(0).await }

async fn pacing(timeout: u64) -> Result {
	let Some(fixture) = fixture(config(false, timeout)).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let mut wakes = WakeQueue::new(); // boot arming state out-param

	for index in [0, 1, 15] {
		let name = format!("paced{index}.example");
		let dest = destination(&name);
		let started = Instant::now();
		let next = sending
			.arm_startup_wake(dest.clone(), index, &mut wakes)
			.await;

		let finished = Instant::now();
		let base_secs = index
			.checked_mul(2)
			.expect("boot pacing delay overflow");

		let base_min = Duration::from_secs(base_secs).max(Duration::from_secs(1));
		let base_max = base_secs
			.checked_add(timeout.saturating_sub(1))
			.map(Duration::from_secs)
			.expect("boot pacing maximum overflow")
			.max(Duration::from_secs(1));

		let jitter_max = Duration::from_secs(base_max.as_secs().max(3).saturating_sub(1));
		let earliest = started
			.checked_add(base_min)
			.expect("minimum wake deadline overflow");

		let latest = finished
			.checked_add(base_max.saturating_add(jitter_max))
			.expect("maximum wake deadline overflow");

		let due = deadline(&wakes, &dest);
		let next_index = index
			.checked_add(1)
			.expect("boot pacing index overflow");

		assert_eq!(next, next_index);
		assert!(due >= earliest, "index={index} due={due:?}");
		assert!(due <= latest, "index={index} due={due:?}");

		let next = sending
			.arm_startup_wake(dest.clone(), next_index, &mut wakes)
			.await;

		assert_eq!(next, next_index);
		assert_eq!(deadline(&wakes, &dest), due);
	}

	assert_eq!(wakes.len(), 3);

	Ok(())
}

#[tokio::test]
async fn closed_gate_and_existing_wake_do_not_advance_boot_pacing() -> Result {
	let Some(fixture) = fixture(config(false, 0)).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let blocked = destination("blocked.example");
	let Destination::Federation(server) = &blocked else {
		unreachable!();
	};

	fixture
		.services
		.federation
		.record_failure(server, Classification::Transient);

	let verdict = fixture
		.services
		.federation
		.should_attempt(server)
		.await;

	assert!(matches!(verdict, ShouldAttempt::No { .. }));

	let armed = destination("armed.example");
	let existing = Instant::now() + Duration::from_secs(100);
	let mut wakes = [Reverse((existing, armed.clone()))].into();
	let index = sending
		.arm_startup_wake(blocked, 0, &mut wakes)
		.await;

	assert_eq!(index, 0);
	let index = sending
		.arm_startup_wake(armed.clone(), index, &mut wakes)
		.await;

	assert_eq!(index, 0);

	assert_eq!(deadline(&wakes, &armed), existing);

	let dest = destination("first-open.example");
	let started = Instant::now();

	let index = sending
		.arm_startup_wake(dest.clone(), index, &mut wakes)
		.await;

	assert_eq!(index, 1);

	assert!(deadline(&wakes, &dest) >= started + Duration::from_secs(1));
	assert!(deadline(&wakes, &dest) < Instant::now() + Duration::from_secs(4));
	assert_eq!(wakes.len(), 3);

	Ok(())
}

#[tokio::test]
async fn overflowing_boot_pace_uses_the_existing_wake_fallback() -> Result {
	let Some(fixture) = fixture(config(false, 0)).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dest = destination("overflow.example");
	let mut wakes = WakeQueue::new(); // boot arming state out-param
	let started = Instant::now();
	let next = sending
		.arm_startup_wake(dest.clone(), u64::MAX, &mut wakes)
		.await;

	let finished = Instant::now();
	let fallback = Duration::from_hours(8760);

	assert_eq!(next, u64::MAX);
	assert!(deadline(&wakes, &dest) >= started + fallback);
	assert!(deadline(&wakes, &dest) <= finished + fallback);

	Ok(())
}
