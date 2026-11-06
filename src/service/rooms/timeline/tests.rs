//! Checks timeline reads and key-boundary behavior.
//!
//! Missing and unreadable rows remain distinct across accepted and outlier
//! storage. Forward scans preserve the lowest valid backfilled count.

use std::{
	pin::pin,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	task::{Wake, Waker},
};

use ruma::{
	RoomId, RoomVersionId, UserId, event_id,
	events::{
		StateEventType,
		room::{
			create::RoomCreateEventContent,
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
		},
	},
	room_id, user_id,
};
use tuwunel_core::{Error, Result, config::Figment, utils::result::NotFound};
use tuwunel_matrix::pdu::PduBuilder;

use super::{Direction, PduCount, Service};
use crate::{
	Services,
	test_utils::{fixture, pdu_id, poll_once},
};

struct RetiredAtStateWrite {
	services: Arc<Services>,
	retired: AtomicU64,
	row_stored: Arc<AtomicBool>,
	row_before_state: AtomicBool,
}

struct TimelineStored(Arc<AtomicBool>);

#[tokio::test]
async fn get_preserves_unreadable_rows() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let timeline = &services.timeline;
	let event = event_id!("$lookup:localhost");
	let id = pdu_id(1);
	let valid: &[u8] = b"{}";
	let corrupt: &[u8] = b"{";
	let cases = [
		(Some(corrupt), None, "corrupt"),
		(None, Some(corrupt), "corrupt"),
		(Some(corrupt), Some(corrupt), "corrupt"),
		(None, None, "missing"),
		(Some(valid), None, "found"),
		(None, Some(valid), "found"),
		(Some(valid), Some(corrupt), "found"),
		(Some(corrupt), Some(valid), "found"),
		(Some(valid), Some(valid), "found"),
	];

	for (accepted, outlier, expected) in cases {
		services.db["eventid_pduid"].remove(event);
		services.db["pduid_pdu"].remove(&id);
		services.db["eventid_outlierpdu"].remove(event);
		if let Some(bytes) = accepted {
			services.db["eventid_pduid"].insert(event, id.as_bytes());
			services.db["pduid_pdu"].insert(&id, bytes);
		}

		if let Some(bytes) = outlier {
			services.db["eventid_outlierpdu"].insert(event, bytes);
		}

		// Warm both lookups so the accepted failure precedes the missing outlier.
		timeline
			.get_non_outlier_pdu_json(event)
			.await
			.ok();

		timeline.get_outlier_pdu_json(event).await.ok();

		let result = timeline.get_pdu_json(event).await.present();
		let outcome = match result {
			| Ok(Some(_)) => "found",
			| Ok(None) => "missing",
			| Err(Error::Json(_)) => "corrupt",
			| Err(error) => return Err(error),
		};

		assert_eq!(outcome, expected, "accepted={accepted:?}, outlier={outlier:?}");
	}

	Ok(())
}

#[test]
fn forward_backfilled_zero_stays_at_lower_bound() {
	let pdu_id = Service::pdu_count_to_id(1, PduCount::Backfilled(0), Direction::Forward);

	assert_eq!(pdu_id.pdu_count(), PduCount::Backfilled(0));
}

#[tokio::test]
async fn accepting_an_invite_installs_state_before_publishing_the_join() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!publication:localhost");
	let creator = user_id!("@creator:localhost");
	let member = user_id!("@member:localhost");

	invite(services, room, creator, member).await?;
	let state_lock = services.state.mutex.lock(room).await;
	let shortroomid = services.short.get_shortroomid(room).await?;
	let row_stored = Arc::new(AtomicBool::new(false));
	let row_waker = Waker::from(Arc::new(TimelineStored(row_stored.clone())));
	let row_watcher =
		pin!(services.db["pduid_pdu"].watch_raw_prefix_once(shortroomid.to_be_bytes()));

	assert!(poll_once(row_watcher, &row_waker).is_pending());

	let observed = Arc::new(RetiredAtStateWrite {
		services: services.clone(),
		// A missing notification must fail the publication assertion too.
		retired: AtomicU64::new(u64::MAX),
		row_stored,
		row_before_state: AtomicBool::new(false),
	});

	let waker = Waker::from(observed.clone());
	let watcher = pin!(services.db["roomid_shortstatehash"].watch_raw_prefix_once(room));
	assert!(poll_once(watcher, &waker).is_pending());

	let builder =
		PduBuilder::state(member.as_str(), &RoomMemberEventContent::new(MembershipState::Join));

	let join = services
		.timeline
		.build_and_append_pdu(builder, member, room, &state_lock)
		.await?;

	let PduCount::Normal(count) = services.timeline.get_pdu_count(&join).await? else {
		panic!("join must have an accepted timeline position");
	};

	assert!(
		observed.row_before_state.load(Ordering::SeqCst),
		"current state referenced a join before its timeline row existed"
	);
	assert!(
		observed.retired.load(Ordering::SeqCst) < count,
		"join was published before its current state was installed"
	);

	let current = services
		.state_accessor
		.room_state_get_id(room, &StateEventType::RoomMember, member.as_str())
		.await?;

	assert_eq!(current, join);
	assert!(services.globals.current_count() >= count);
	Ok(())
}

async fn invite(services: &Services, room: &RoomId, creator: &UserId, member: &UserId) -> Result {
	let _short_id = services
		.short
		.get_or_create_shortroomid(room)
		.await;

	let state_lock = services.state.mutex.lock(room).await;
	let create = RoomCreateEventContent {
		room_version: RoomVersionId::V11,
		..RoomCreateEventContent::new_v11()
	};

	let join =
		PduBuilder::state(creator.as_str(), &RoomMemberEventContent::new(MembershipState::Join));

	let rules = PduBuilder::state("", &RoomJoinRulesEventContent::new(JoinRule::Invite));
	let invite =
		PduBuilder::state(member.as_str(), &RoomMemberEventContent::new(MembershipState::Invite));

	for (sender, builder) in [
		(creator, PduBuilder::state("", &create)),
		(creator, join),
		(creator, rules),
		(creator, invite),
	] {
		services
			.timeline
			.build_and_append_pdu(builder, sender, room, &state_lock)
			.await?;
	}

	Ok(())
}

impl Wake for RetiredAtStateWrite {
	fn wake(self: Arc<Self>) { self.wake_by_ref(); }

	fn wake_by_ref(self: &Arc<Self>) {
		self.row_before_state
			.store(self.row_stored.load(Ordering::SeqCst), Ordering::SeqCst);

		self.retired
			.store(self.services.globals.current_count(), Ordering::SeqCst);
	}
}

impl Wake for TimelineStored {
	fn wake(self: Arc<Self>) { self.0.store(true, Ordering::SeqCst); }
}
