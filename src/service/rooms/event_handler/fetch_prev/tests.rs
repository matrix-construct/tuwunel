use ruma::{event_id, owned_event_id};

use super::{MISSING_EVENTS_LIMIT, PREV_FETCH_WIDTH, PrevQueue, missing_events};

#[test]
fn queued_and_started_events_spend_one_walk_allowance() {
	let mut queue = PrevQueue::new([owned_event_id!("$root")].into(), 2);
	assert!(queue.admit(event_id!("$a")), "first predecessor fits");
	assert_eq!(queue.take_ready(0).count(), 2, "both queued events start");
	assert!(queue.admit(event_id!("$b")), "last predecessor fits");
	assert!(!queue.admit(event_id!("$c")), "starting a fetch does not refund it");
}

#[test]
fn waiting_events_use_only_free_slots_in_order() {
	let a = owned_event_id!("$a");
	let b = owned_event_id!("$b");
	let mut queue = PrevQueue::new([a.clone(), b.clone()].into(), 2);
	assert_eq!(queue.take_ready(PREV_FETCH_WIDTH).count(), 0, "full lane");
	assert_eq!(queue.take_ready(31).collect::<Vec<_>>(), [a], "one slot was freed");
	assert_eq!(queue.take_ready(0).collect::<Vec<_>>(), [b], "the tail is not dropped");
}

#[test]
fn missing_events_stop_at_the_requested_count() {
	let batch = b"[1,2,3,4,5,6,7,8,9,10,11]";
	let events: Vec<_> = missing_events(batch)
		.expect("valid batch")
		.map(serde_json::value::RawValue::get)
		.collect();
	assert_eq!(events.len(), MISSING_EVENTS_LIMIT.get(), "only requested events land");
	assert_eq!(events.last(), Some(&"10"), "keep response order");
}
