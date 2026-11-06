//! Checks timeline boundaries and state visibility at stream publication.

use std::{
	pin::pin,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	task::{Wake, Waker},
};

use ruma::{
	RoomId, RoomVersionId, UserId,
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
use tuwunel_core::{Result, config::Figment};
use tuwunel_matrix::pdu::PduBuilder;

use super::{Direction, PduCount, Service};
use crate::{
	Services,
	test_utils::{fixture, poll_once},
};

#[test]
fn forward_backfilled_zero_stays_at_lower_bound() {
	let pdu_id = Service::pdu_count_to_id(1, PduCount::Backfilled(0), Direction::Forward);

	assert_eq!(pdu_id.pdu_count(), PduCount::Backfilled(0));
}

struct RetiredAtStateWrite {
	services: Arc<Services>,
	retired: AtomicU64,
	row_stored: Arc<AtomicBool>,
	row_before_state: AtomicBool,
}

struct TimelineStored(Arc<AtomicBool>);

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
	let mut row_watcher =
		pin!(services.db["pduid_pdu"].watch_raw_prefix_once(shortroomid.to_be_bytes()));
	assert!(poll_once(row_watcher.as_mut(), &row_waker).is_pending());

	let observed = Arc::new(RetiredAtStateWrite {
		services: services.clone(),
		// A missing notification must fail the publication assertion too.
		retired: AtomicU64::new(u64::MAX),
		row_stored,
		row_before_state: AtomicBool::new(false),
	});
	let waker = Waker::from(observed.clone());
	let mut watcher = pin!(services.db["roomid_shortstatehash"].watch_raw_prefix_once(room));
	assert!(poll_once(watcher.as_mut(), &waker).is_pending());

	let join = services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				member.as_str(),
				&RoomMemberEventContent::new(MembershipState::Join),
			),
			member,
			room,
			&state_lock,
		)
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
	for (sender, builder) in [
		(creator, PduBuilder::state("", &create)),
		(
			creator,
			PduBuilder::state(
				creator.as_str(),
				&RoomMemberEventContent::new(MembershipState::Join),
			),
		),
		(
			creator,
			PduBuilder::state("", &RoomJoinRulesEventContent::new(JoinRule::Invite)),
		),
		(
			creator,
			PduBuilder::state(
				member.as_str(),
				&RoomMemberEventContent::new(MembershipState::Invite),
			),
		),
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
