#![cfg(test)]

use ruma::{RoomId, UserId};
use tuwunel_core::matrix::pdu::PduCount;
use tuwunel_database::{Interfix, SEP, serialize_to_vec};

use super::{data::position_advances, thread_kind_to_receipt};

const ROOM: &str = "!room:example.com";
const USER: &str = "@user:example.com";
const THREAD_ROOT: &str = "$thread_root_event_id";
const COUNT: u64 = 42;

fn room() -> &'static RoomId { ROOM.try_into().unwrap() }
fn user() -> &'static UserId { USER.try_into().unwrap() }

fn legacy_key() -> Vec<u8> {
	serialize_to_vec((room(), COUNT, user())).expect("serialize legacy key")
}

fn new_key(kind: &str) -> Vec<u8> {
	serialize_to_vec((room(), COUNT, user(), kind)).expect("serialize new key")
}

#[test]
fn unthreaded_new_key_is_legacy_plus_separator() {
	let legacy = legacy_key();
	let new = new_key("");

	assert_eq!(&new[..legacy.len()], &*legacy);
	assert_eq!(new.len(), legacy.len() + 1);
	assert_eq!(*new.last().unwrap(), SEP);
}

#[test]
fn main_key_appends_kind_after_separator() {
	let legacy = legacy_key();
	let main = new_key("main");

	assert_eq!(&main[..legacy.len()], &*legacy);
	assert_eq!(main[legacy.len()], SEP);
	assert_eq!(&main[legacy.len() + 1..], b"main");
}

#[test]
fn thread_key_appends_root_after_separator() {
	let legacy = legacy_key();
	let thread = new_key(THREAD_ROOT);

	assert_eq!(&thread[..legacy.len()], &*legacy);
	assert_eq!(thread[legacy.len()], SEP);
	assert_eq!(&thread[legacy.len() + 1..], THREAD_ROOT.as_bytes());
}

/// Sweep filter behavior under each (stored_kind, sweep_kind) pairing.
/// Mirrors the `key.ends_with(suffix) || (legacy_match && key.ends_with(user))`
/// check in `data::readreceipt_update`.
#[test]
fn sweep_filter_matrix() {
	let user_bytes = user().as_bytes();
	let suffix = |kind: &str| serialize_to_vec((user(), kind)).expect("serialize suffix");

	let matches = |stored: &[u8], sweep_kind: &str| -> bool {
		let s = suffix(sweep_kind);
		let legacy_match = sweep_kind.is_empty();

		stored.ends_with(&s) || (legacy_match && stored.ends_with(user_bytes))
	};

	let legacy = legacy_key();
	let empty = new_key("");
	let main = new_key("main");
	let thread_a = new_key(THREAD_ROOT);
	let thread_b = new_key("$other_root");

	assert!(matches(&legacy, ""), "Unthreaded sweep catches legacy 3-tuple row");
	assert!(matches(&empty, ""), "Unthreaded sweep catches own-shape row");
	assert!(!matches(&main, ""), "Unthreaded sweep skips Main row");
	assert!(!matches(&thread_a, ""), "Unthreaded sweep skips Thread row");

	assert!(!matches(&legacy, "main"), "Main sweep does not touch legacy row");
	assert!(!matches(&empty, "main"), "Main sweep does not touch Unthreaded row");
	assert!(matches(&main, "main"), "Main sweep catches Main row");
	assert!(!matches(&thread_a, "main"), "Main sweep does not touch Thread row");

	assert!(!matches(&legacy, THREAD_ROOT), "Thread sweep does not touch legacy row");
	assert!(!matches(&empty, THREAD_ROOT), "Thread sweep does not touch Unthreaded row");
	assert!(!matches(&main, THREAD_ROOT), "Thread sweep does not touch Main row");
	assert!(matches(&thread_a, THREAD_ROOT), "Thread sweep catches matching root");
	assert!(!matches(&thread_b, THREAD_ROOT), "Thread sweep does not touch other root");
}

/// `user_id` ends with `:example.com`; the bare-user-id `ends_with` check
/// must not collide with kind tails. Encoded `"main"` and event-id roots
/// (`$...`) end in different bytes than a server-name suffix.
#[test]
fn legacy_match_does_not_collide_with_kind_tails() {
	let user_bytes = user().as_bytes();

	assert!(!new_key("main").ends_with(user_bytes));
	assert!(!new_key(THREAD_ROOT).ends_with(user_bytes));
	assert!(!new_key("").ends_with(user_bytes), "empty kind ends with SEP, not user_id");
	assert!(legacy_key().ends_with(user_bytes));
}

/// Position comparison behind the public receipt gate.
///
/// A stored position of `None` means no row was found. A position of `None`
/// after resolution means the event is not in this server's timeline.
#[test]
fn position_advance_matrix() {
	let normal = |count| Some(PduCount::Normal(count));
	let backfilled = |count| Some(PduCount::Backfilled(count));

	assert!(position_advances(None, normal(1)), "absent stored position accepts");
	assert!(position_advances(None, None), "neither position resolved accepts");
	assert!(position_advances(normal(2), None), "unresolvable incoming accepts");
	assert!(position_advances(normal(1), normal(2)), "strictly greater advances");
	assert!(!position_advances(normal(2), normal(2)), "equal position rejects");
	assert!(!position_advances(normal(3), normal(2)), "lower position rejects");
	assert!(position_advances(backfilled(-1), normal(1)), "normal advances past backfilled");
	assert!(!position_advances(normal(1), backfilled(-1)), "backfilled sorts below normal");
}

#[test]
fn invalid_thread_kind_is_rejected() {
	thread_kind_to_receipt("not-an-event-id")
		.expect_err("invalid private receipt thread must fail");
}

/// MSC3771 per-thread `m.read.private` storage. `roomuserid_privateread`
/// stores the unthreaded marker as a 2-tuple `(room, user)` (legacy shape,
/// unchanged) and per-thread markers as 3-tuple `(room, user, kind)` rows.
/// The Interfix prefix excludes the legacy 2-tuple by construction so a
/// sweep of thread rows leaves the unthreaded marker intact.
mod private_read {
	use super::*;

	fn legacy_2tuple() -> Vec<u8> {
		serialize_to_vec((room(), user())).expect("serialize legacy 2-tuple key")
	}

	fn thread_3tuple(kind: &str) -> Vec<u8> {
		serialize_to_vec((room(), user(), kind)).expect("serialize 3-tuple key")
	}

	fn thread_prefix() -> Vec<u8> {
		serialize_to_vec((room(), user(), Interfix)).expect("serialize thread prefix")
	}

	#[test]
	fn legacy_2tuple_and_3tuple_are_byte_distinct() {
		let legacy = legacy_2tuple();
		let unthreaded = thread_3tuple("");
		let main = thread_3tuple("main");
		let thread = thread_3tuple(THREAD_ROOT);

		assert_ne!(legacy, unthreaded);
		assert_ne!(legacy, main);
		assert_ne!(legacy, thread);
		assert_eq!(&main[..legacy.len()], &*legacy);
		assert_eq!(main[legacy.len()], SEP);
	}

	#[test]
	fn interfix_prefix_excludes_legacy_2tuple_row() {
		let prefix = thread_prefix();
		let legacy = legacy_2tuple();

		assert!(
			!legacy.starts_with(&prefix),
			"legacy 2-tuple has no trailing SEP; cannot match thread prefix"
		);
		assert_eq!(prefix.len(), legacy.len() + 1);
		assert_eq!(&prefix[..legacy.len()], &*legacy);
		assert_eq!(*prefix.last().unwrap(), SEP);
	}

	#[test]
	fn interfix_prefix_includes_thread_rows() {
		let prefix = thread_prefix();

		assert!(thread_3tuple("").starts_with(&prefix));
		assert!(thread_3tuple("main").starts_with(&prefix));
		assert!(thread_3tuple(THREAD_ROOT).starts_with(&prefix));
	}

	#[test]
	fn distinct_thread_kinds_have_distinct_keys() {
		assert_ne!(thread_3tuple("main"), thread_3tuple(THREAD_ROOT));
		assert_ne!(thread_3tuple(THREAD_ROOT), thread_3tuple("$other_root"));
	}

	#[test]
	fn thread_sweep_preserves_legacy_unthreaded_row() {
		let prefix = thread_prefix();
		let legacy = legacy_2tuple();
		let main = thread_3tuple("main");
		let thread = thread_3tuple(THREAD_ROOT);

		assert!(!legacy.starts_with(&prefix));
		assert!(main.starts_with(&prefix));
		assert!(thread.starts_with(&prefix));
	}
}
