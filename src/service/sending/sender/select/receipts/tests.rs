use std::sync::atomic::{AtomicUsize, Ordering};

use ruma::{
	EventId, OwnedUserId, UserId, api::federation::transactions::edu::ReceiptData,
	events::receipt::Receipt, owned_user_id,
};
use tuwunel_core::utils::stream::IterStream;

use super::{USER_LIMIT, rank_receipts};

#[tokio::test]
async fn interleaved_users_rank_in_arrival_order() {
	let alice = owned_user_id!("@alice:example.com");
	let bob = owned_user_id!("@bob:example.com");
	let receipts = [receipt(&alice, 1), receipt(&bob, 2), receipt(&alice, 3), receipt(&alice, 4)];

	let num = AtomicUsize::new(0);
	let ranked = rank_receipts(receipts.stream(), &num).await;

	assert_eq!(ranked.len(), 3);
	assert_eq!(ranked[0].read.len(), 2);
	assert_eq!(ranked[1].read.len(), 1);
	assert_eq!(ranked[2].read.len(), 1);
	assert_eq!(ranked[0].read[&bob].event_ids[0].as_str(), "$event2");
	assert_eq!(ranked[0].read[&alice].event_ids[0].as_str(), "$event1");
	assert_eq!(ranked[1].read[&alice].event_ids[0].as_str(), "$event3");
	assert_eq!(ranked[2].read[&alice].event_ids[0].as_str(), "$event4");
	assert_eq!(num.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn one_user_occupies_a_prefix_of_ranks() {
	let alice = owned_user_id!("@alice:example.com");
	let receipts = (1..=64).map(|n| receipt(&alice, n));

	let num = AtomicUsize::new(0);
	let ranked = rank_receipts(receipts.stream(), &num).await;

	assert_eq!(ranked.len(), 64);
	for (rank, map) in ranked.iter().enumerate() {
		let expected = format!("$event{}", rank.saturating_add(1));

		assert_eq!(map.read.len(), 1);
		assert_eq!(map.read[&alice].event_ids[0].as_str(), expected);
	}

	assert_eq!(num.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn distinct_users_stop_at_the_budget() {
	let receipts = (0..USER_LIMIT.saturating_mul(2)).map(|n| {
		let user_id = OwnedUserId::parse(format!("@user{n}:example.com")).expect("valid user id");

		receipt(&user_id, 1)
	});

	let num = AtomicUsize::new(0);
	let ranked = rank_receipts(receipts.stream(), &num).await;

	// The crossing user ships, so the shipped count is one past the limit.
	assert_eq!(ranked.len(), 1);
	assert_eq!(ranked[0].read.len(), USER_LIMIT.saturating_add(1));
	assert_eq!(num.load(Ordering::Relaxed), USER_LIMIT.saturating_add(1));
}

fn receipt(user_id: &UserId, n: u64) -> (OwnedUserId, ReceiptData) {
	let event_id = EventId::parse(format!("$event{n}")).expect("valid event id");
	let data = ReceiptData {
		data: Receipt::default(),
		event_ids: vec![event_id],
	};

	(user_id.to_owned(), data)
}
