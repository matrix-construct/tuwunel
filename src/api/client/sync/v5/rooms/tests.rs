use std::{collections::HashSet, iter::once};

use ruma::{
	UInt,
	api::client::sync::sync_events::v5::response::Room as ResponseRoom,
	events::{StateEventType, room::member::MembershipState},
	uint, user_id,
};
use tuwunel_core::matrix::pdu::PduCount;

use super::{
	StateMode, membership_allows_required_state, required_state_hash, room_config,
	room_timeline_limited, room_timeline_metadata, state_is_required, state_may_have_changed,
	state_mode, state_was_requested,
};

#[test]
fn first_connection_timeline_is_initial_and_historical() {
	let (initial, num_live) = room_timeline_metadata(0, None, &timeline(&[8, 9, 10]));

	assert_eq!(initial, Some(true));
	assert_eq!(num_live, None);
}

#[test]
fn incremental_new_room_has_one_live_event() {
	let (initial, num_live) = room_timeline_metadata(0, Some(10), &timeline(&[8, 9, 11]));

	assert_eq!(initial, Some(true));
	assert_eq!(num_live, Some(uint!(1)));
}

#[test]
fn incremental_range_expansion_has_no_live_events() {
	let (initial, num_live) = room_timeline_metadata(0, Some(10), &timeline(&[7, 8, 9]));

	assert_eq!(initial, Some(true));
	assert_eq!(num_live, Some(uint!(0)));
}

#[test]
fn incremental_timeline_counts_only_live_suffix() {
	let (initial, num_live) = room_timeline_metadata(5, Some(10), &timeline(&[8, 9, 11, 12]));

	assert_eq!(initial, None);
	assert_eq!(num_live, Some(uint!(2)));
}

#[test]
fn limited_timeline_counts_only_returned_live_events() {
	// Earlier live events at positions 11 through 13 were truncated.
	let returned_timeline = timeline(&[14, 15]);
	let (_, num_live) = room_timeline_metadata(5, Some(10), &returned_timeline);

	assert_eq!(num_live, Some(uint!(2)));
	let timeline_len =
		UInt::try_from(returned_timeline.len()).expect("timeline length fits UInt");

	assert!(num_live.expect("incremental response") <= timeline_len);
}

#[test]
fn required_state_is_limited_to_visible_memberships() {
	assert!(membership_allows_required_state(None));
	assert!(membership_allows_required_state(Some(&MembershipState::Join)));
	assert!(!membership_allows_required_state(Some(&MembershipState::Invite)));
	assert!(!membership_allows_required_state(Some(&MembershipState::Knock)));
}

#[test]
fn required_state_is_full_initially_and_without_coverage() {
	assert_eq!(state_mode(0, false), StateMode::Full);
	assert_eq!(state_mode(7, true), StateMode::Full);
	assert_eq!(state_mode(7, false), StateMode::Delta(PduCount::Normal(7)));
	assert!(state_is_required(StateMode::Full, Some(PduCount::Normal(1)), false, true));
}

#[test]
fn incremental_required_state_omits_unchanged_events() {
	let mode = StateMode::Delta(PduCount::Normal(7));

	assert!(!state_is_required(mode, Some(PduCount::Normal(7)), false, true));
	assert!(!state_is_required(mode, Some(PduCount::Normal(6)), false, true));
}

#[test]
fn incremental_required_state_includes_only_changes() {
	let mode = StateMode::Delta(PduCount::Normal(7));
	let included = [PduCount::Normal(6), PduCount::Normal(8)]
		.into_iter()
		.filter(|count| state_is_required(mode, Some(*count), false, true))
		.count();

	assert_eq!(included, 1);
}

#[test]
fn incremental_required_state_keeps_reselected_old_event() {
	let mode = StateMode::Delta(PduCount::Normal(7));

	assert!(state_is_required(mode, Some(PduCount::Normal(6)), false, false));
}

#[test]
fn incremental_required_state_keeps_uncounted_events() {
	let mode = StateMode::Delta(PduCount::Normal(7));

	assert!(state_is_required(mode, None, false, false));
}

#[test]
fn incremental_required_state_needs_a_newer_timeline_event() {
	let mode = StateMode::Delta(PduCount::Normal(7));

	assert!(!state_may_have_changed(mode, PduCount::Normal(7)));
	assert!(state_may_have_changed(mode, PduCount::Normal(8)));
	assert!(state_may_have_changed(StateMode::Full, PduCount::Normal(0)));
}

#[test]
fn incremental_required_state_keeps_lazy_members() {
	let mode = StateMode::Delta(PduCount::Normal(7));

	assert!(state_is_required(mode, Some(PduCount::Normal(1)), true, true));
}

#[test]
fn empty_required_state_is_omitted() {
	let room = serde_json::to_value(ResponseRoom::new()).expect("room must serialize");

	assert!(room.get("required_state").is_none());
}

#[test]
fn zero_timeline_limit_is_not_limited() {
	assert!(!room_timeline_limited(0, true));
	assert!(room_timeline_limited(1, true));
	assert!(!room_timeline_limited(1, false));
}

#[test]
fn config_hash_is_order_independent() {
	let first: HashSet<_> =
		[(StateEventType::RoomName, "".into()), (StateEventType::RoomMember, "*".into())].into();

	let second: HashSet<_> =
		[(StateEventType::RoomMember, "*".into()), (StateEventType::RoomName, "".into())].into();

	assert_eq!(room_config(&(0, first)).0, room_config(&(0, second)).0);
}

#[test]
fn config_hash_uses_deduplicated_state() {
	let entry = (StateEventType::RoomName, "".into());
	let duplicated = [entry.clone(), entry.clone()]
		.into_iter()
		.collect();

	let deduplicated = once(entry).collect();

	assert_eq!(room_config(&(0, duplicated)).0, room_config(&(0, deduplicated)).0);
}

#[test]
fn config_hash_tracks_timeline_limit() {
	let empty = HashSet::new();

	assert_ne!(room_config(&(0, empty.clone())).0, 0);
	assert_ne!(room_config(&(0, empty.clone())).0, room_config(&(1, empty)).0);
}

#[test]
fn state_coverage_resolves_wildcards_and_own_user() {
	let user = user_id!("@share:example.com");
	let kind = StateEventType::BeaconInfo;

	for key in ["*", "$ME", user.as_str()] {
		let previous = [required_state_hash(&kind, key)];

		assert!(state_was_requested(&previous, &kind, user.as_str(), user));
	}

	let previous = [required_state_hash(&kind, "$ME")];

	assert!(!state_was_requested(&previous, &kind, "@other:example.com", user));
	assert!(!state_was_requested(
		&previous,
		&StateEventType::RoomMember,
		user.as_str(),
		user
	));
}

#[test]
fn lazy_membership_does_not_claim_general_state_coverage() {
	let user = user_id!("@share:example.com");
	let kind = StateEventType::RoomMember;
	let previous = [required_state_hash(&kind, "$LAZY")];
	let details = (0, [(kind.clone(), "$LAZY".into())].into());

	assert!(room_config(&details).1.is_empty());
	assert!(!state_was_requested(&previous, &kind, user.as_str(), user));
}

fn timeline(positions: &[u64]) -> Vec<(PduCount, ())> {
	positions
		.iter()
		.copied()
		.map(|position| (PduCount::Normal(position), ()))
		.collect()
}
