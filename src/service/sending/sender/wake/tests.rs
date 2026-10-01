use std::{cmp::Reverse, time::Duration};

use tokio::time::Instant;

use super::{WakeQueue, appservice_delay, arm_appservice_wake};
use crate::sending::Destination;

#[test]
fn appservice_curve_clamps_before_exponentiation() {
	for (tries, seconds) in [
		(0, 1),
		(1, 2),
		(2, 4),
		(8, 256),
		(9, 512),
		(10, 512),
		(32, 512),
		(u32::MAX, 512),
	] {
		assert_eq!(appservice_delay(tries), Duration::from_secs(seconds));
	}
}

#[test]
fn appservice_failure_wakes_use_jitter_and_dedupe_existing_entries() {
	for (tries, seconds) in [(1, 2), (2, 4), (9, 512), (u32::MAX, 512)] {
		let dest = Destination::Appservice(format!("bridge{tries}"));
		let mut wakes = WakeQueue::new(); // wake arming state out-param
		let started = Instant::now();

		arm_appservice_wake(&mut wakes, dest.clone(), tries);

		let finished = Instant::now();
		let Reverse((due, armed)) = wakes.peek().expect("failure wake");
		let due = *due;
		let maximum = seconds + seconds.max(3) - 1;

		assert_eq!(armed, &dest);
		assert!(due >= started + Duration::from_secs(seconds));
		assert!(due <= finished + Duration::from_secs(maximum));

		arm_appservice_wake(&mut wakes, dest, tries.saturating_add(1));

		assert_eq!(wakes.len(), 1);
		assert_eq!(wakes.peek().expect("same failure wake").0.0, due);
	}
}
