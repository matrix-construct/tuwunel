//! In-memory "last changed at" index for incremental `/sync`.
//!
//! `sync_events_route` (api/client/sync/v3.rs) rebuilds an incremental
//! response on entry whenever the GLOBAL counter has moved past the client's
//! `since`, and only then waits on the user's watchers. On a server where
//! anything at all is being written that is every poll of every connected
//! client, and each rebuild walks every room the user is in with a dozen
//! RocksDB lookups apiece: ~3.8 ms + ~0.28 ms per room on a 4-vCPU primary.
//! Load-tested (netsapiens-matrix `hold`, 2026-09-30): an idle primary held
//! 44,000 connections; with 1 msg/s of chat elsewhere, 15,000.
//!
//! This index answers "did anything this user's sync reads change after
//! `since`?" from memory, so the route can skip a rebuild that would come back
//! empty. Every write to a map that `/sync` watches (see `watch.rs`), plus the
//! private-read maps that it reads but does not watch, stamps the user, room or
//! short room it belongs to with the counter's DISPATCHED value at that moment.
//!
//! Why that is safe: a writer holds its counter permit until its write is done,
//! so dispatched-at-write-time is >= the write's own count. A write with count
//! `c > since` therefore always leaves a stamp `> since`. Over-stamping only
//! costs an unneeded rebuild. The hook runs inside `Map::notify`, before the
//! watchers are woken, so anything a watcher wakes sees the stamp.
//!
//! What it does NOT see, and how that is handled:
//! - Writes before `enable()` or in a process that is, or was, a replica (WAL
//!   frames are applied raw and never notify). The index is only enabled in a
//!   process that started as primary, and `valid_from` is read after enabling,
//!   so a `since` older than that always rebuilds.
//! - A key whose layout does not parse. Stamped into `unattributed`, which
//!   makes every `since` below it rebuild.
//! - A demotion. From then on this process applies another primary's WAL raw,
//!   so `retire()` switches the index off for the rest of its life, even if it
//!   is promoted again in place. (Today `replication::demote` refuses unless
//!   the process was promoted in place, which never enables the index; this
//!   keeps that true if demote ever learns to demote a standalone primary.)

use std::{
	collections::HashMap,
	hash::Hash,
	sync::{
		RwLock,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
};

/// How a watched map's key names the user or room a write belongs to. Each
/// mirrors a watch prefix in `watch.rs` (tuples are separated by 0xFF; an
/// absent `Option<&RoomId>` serializes as empty).
#[derive(Clone, Copy, Debug)]
pub(super) enum Layout {
	/// `user 0xFF ...`, or exactly `user`.
	User,
	/// `[room] 0xFF user [0xFF ...]`; the room part may be empty.
	SecondIsUser,
	/// `user 0xFF ...` or `room 0xFF ...` (keychangeid_userid).
	UserOrRoom,
	/// `room 0xFF ...`.
	Room,
	/// `shortroomid (8 bytes, big-endian) ...` (pduid_pdu).
	ShortRoom,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Target<'a> {
	User(&'a [u8]),
	Room(&'a [u8]),
	ShortRoom(u64),
	Unknown,
}

const SEP: u8 = 0xFF;

fn user(part: &[u8]) -> Target<'_> {
	if part.first() == Some(&b'@') {
		Target::User(part)
	} else {
		Target::Unknown
	}
}

fn room(part: &[u8]) -> Target<'_> {
	if part.first() == Some(&b'!') {
		Target::Room(part)
	} else {
		Target::Unknown
	}
}

/// Which user or room a key written to a map of `layout` belongs to. Anything
/// that does not look like the expected layout is `Unknown`, never a guess.
pub(super) fn target(layout: Layout, key: &[u8]) -> Target<'_> {
	let mut parts = key.split(|&b| b == SEP);
	let first = parts.next().unwrap_or_default();
	match layout {
		| Layout::User => user(first),
		| Layout::SecondIsUser => parts.next().map_or(Target::Unknown, user),
		| Layout::UserOrRoom => match first.first() {
			| Some(b'@') => Target::User(first),
			| Some(b'!') => Target::Room(first),
			| _ => Target::Unknown,
		},
		| Layout::Room => room(first),
		| Layout::ShortRoom => key
			.get(..8)
			.and_then(|b| b.try_into().ok())
			.map_or(Target::Unknown, |b| Target::ShortRoom(u64::from_be_bytes(b))),
	}
}

pub(super) struct ChangeIndex {
	enabled: AtomicBool,
	/// Set once, by `retire()`; `enable()` is then a no-op.
	retired: AtomicBool,
	/// Oldest `since` the index can answer for. u64::MAX until enabled.
	valid_from: AtomicU64,
	/// Highest stamp of a write whose key did not parse.
	unattributed: AtomicU64,
	users: RwLock<HashMap<Box<[u8]>, u64>>,
	rooms: RwLock<HashMap<Box<[u8]>, u64>>,
	shortrooms: RwLock<HashMap<u64, u64>>,
}

impl Default for ChangeIndex {
	fn default() -> Self {
		Self {
			enabled: AtomicBool::new(false),
			retired: AtomicBool::new(false),
			valid_from: AtomicU64::new(u64::MAX),
			unattributed: AtomicU64::new(0),
			users: RwLock::default(),
			rooms: RwLock::default(),
			shortrooms: RwLock::default(),
		}
	}
}

fn bump<K: Eq + Hash>(map: &RwLock<HashMap<K, u64>>, key: K, count: u64) {
	let mut map = map.write().expect("change index lock poisoned");
	map.entry(key)
		.and_modify(|c| *c = (*c).max(count))
		.or_insert(count);
}

fn bump_bytes(map: &RwLock<HashMap<Box<[u8]>, u64>>, key: &[u8], count: u64) {
	{
		let mut map = map.write().expect("change index lock poisoned");
		if let Some(c) = map.get_mut(key) {
			*c = (*c).max(count);
			return;
		}
	}
	bump(map, key.into(), count);
}

fn newer(map: &RwLock<HashMap<Box<[u8]>, u64>>, key: &[u8], since: u64) -> bool {
	map.read()
		.expect("change index lock poisoned")
		.get(key)
		.is_some_and(|&c| c > since)
}

impl ChangeIndex {
	#[inline]
	pub(super) fn is_enabled(&self) -> bool { self.enabled.load(Ordering::Acquire) }

	/// Start answering. `dispatched` is read only after the hooks are live, so
	/// any write they missed has a count at or below `valid_from`.
	pub(super) fn enable(&self, dispatched: impl FnOnce() -> u64) {
		if self.retired.load(Ordering::SeqCst) {
			return;
		}
		self.enabled.store(true, Ordering::SeqCst);
		self.valid_from
			.store(dispatched(), Ordering::SeqCst);
	}

	/// Stop answering, for good: every `since` rebuilds from now on.
	pub(super) fn retire(&self) {
		self.retired.store(true, Ordering::SeqCst);
		self.valid_from.store(u64::MAX, Ordering::SeqCst);
		self.enabled.store(false, Ordering::SeqCst);
	}

	/// Record a write to a map of `layout`, stamped with `count` (the counter's
	/// dispatched value at write time). No-op until enabled.
	pub(super) fn record(&self, layout: Layout, key: &[u8], count: u64) {
		if !self.is_enabled() {
			return;
		}
		match target(layout, key) {
			| Target::User(u) => bump_bytes(&self.users, u, count),
			| Target::Room(r) => bump_bytes(&self.rooms, r, count),
			| Target::ShortRoom(s) => bump(&self.shortrooms, s, count),
			| Target::Unknown => {
				self.unattributed
					.fetch_max(count, Ordering::AcqRel);
			},
		}
	}

	/// Whether the index can vouch for "nothing changed since `since`" at all.
	pub(super) fn covers(&self, since: u64) -> bool {
		self.is_enabled()
			&& since >= self.valid_from.load(Ordering::SeqCst)
			&& self.unattributed.load(Ordering::Acquire) <= since
	}

	pub(super) fn user_changed(&self, user_id: &[u8], since: u64) -> bool {
		newer(&self.users, user_id, since)
	}

	pub(super) fn room_changed(
		&self,
		room_id: &[u8],
		shortroomid: Option<u64>,
		since: u64,
	) -> bool {
		newer(&self.rooms, room_id, since)
			|| shortroomid.is_some_and(|s| {
				self.shortrooms
					.read()
					.expect("change index lock poisoned")
					.get(&s)
					.is_some_and(|&c| c > since)
			})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn key(parts: &[&[u8]]) -> Vec<u8> { parts.join(&SEP) }

	#[test]
	fn targets_follow_the_watch_prefixes() {
		let u = b"@alice:hs".as_slice();
		let r = b"!room:hs".as_slice();
		let count = 7_u64.to_be_bytes();
		// userroomid_*: user 0xFF room
		assert_eq!(target(Layout::User, &key(&[u, r])), Target::User(u));
		// userid_lastonetimekeyupdate: exactly the user
		assert_eq!(target(Layout::User, u), Target::User(u));
		// roomusertype_roomuserdataid, global: "" 0xFF user 0xFF type
		assert_eq!(
			target(Layout::SecondIsUser, &key(&[b"", u, b"m.push_rules"])),
			Target::User(u)
		);
		// ... and per room: room 0xFF user 0xFF type
		assert_eq!(target(Layout::SecondIsUser, &key(&[r, u, b"m.tag"])), Target::User(u));
		// roomuserdataid_accountdata: [room] 0xFF user 0xFF count 0xFF type
		assert_eq!(target(Layout::SecondIsUser, &key(&[b"", u, &count, b"t"])), Target::User(u));
		// roomuserid_lastnotificationread / privateread: room 0xFF user
		assert_eq!(target(Layout::SecondIsUser, &key(&[r, u])), Target::User(u));
		// keychangeid_userid: user|room 0xFF count
		assert_eq!(target(Layout::UserOrRoom, &key(&[u, &count])), Target::User(u));
		assert_eq!(target(Layout::UserOrRoom, &key(&[r, &count])), Target::Room(r));
		// readreceiptid_readreceipt: room 0xFF count 0xFF user
		assert_eq!(target(Layout::Room, &key(&[r, &count, u])), Target::Room(r));
		// pduid_pdu: shortroomid ++ count, no separator
		let mut pduid = 42_u64.to_be_bytes().to_vec();
		pduid.extend(0xFFFF_u64.to_be_bytes());
		assert_eq!(target(Layout::ShortRoom, &pduid), Target::ShortRoom(42));
	}

	/// Keys built with the database serializer, exactly as the writers build
	/// them (account_data, read_receipt, pusher, users, state_cache), so a
	/// layout change there fails here instead of silently skipping syncs.
	#[test]
	fn keys_from_the_real_serializer_parse() {
		use ruma::{owned_room_id, owned_user_id};
		use tuwunel_database::{Interfix, serialize_key};

		let u = owned_user_id!("@alice:hs");
		let r = owned_room_id!("!room:hs");
		let ub = u.as_bytes();
		let rb = r.as_bytes();
		let none: Option<&ruma::RoomId> = None;

		// account_data::update: (room_id, user_id, count, type) and (room_id, user_id,
		// type)
		let k = serialize_key((none, &u, 7_u64, "m.push_rules")).unwrap();
		assert_eq!(target(Layout::SecondIsUser, &k), Target::User(ub));
		let k = serialize_key((Some(&*r), &u, 7_u64, "m.tag")).unwrap();
		assert_eq!(target(Layout::SecondIsUser, &k), Target::User(ub));
		let k = serialize_key((none, &u, "m.push_rules")).unwrap();
		assert_eq!(target(Layout::SecondIsUser, &k), Target::User(ub));
		// pusher / read_receipt private reads: (room_id, user_id)
		let k = serialize_key((&r, &u)).unwrap();
		assert_eq!(target(Layout::SecondIsUser, &k), Target::User(ub));
		// userroomid_*: (user_id, room_id)
		let k = serialize_key((&u, &r)).unwrap();
		assert_eq!(target(Layout::User, &k), Target::User(ub));
		// todeviceid_events: (user_id, device_id, count)
		let k = serialize_key((&u, "DEVICE", 0xFFFF_FFFF_u64)).unwrap();
		assert_eq!(target(Layout::User, &k), Target::User(ub));
		// keychangeid_userid: (user_id | room_id, count)
		let k = serialize_key((&u, 9_u64)).unwrap();
		assert_eq!(target(Layout::UserOrRoom, &k), Target::User(ub));
		let k = serialize_key((&r, 9_u64)).unwrap();
		assert_eq!(target(Layout::UserOrRoom, &k), Target::Room(rb));
		// readreceiptid_readreceipt: (room_id, count, user_id)
		let k = serialize_key((&r, 0x00FF_u64, &u)).unwrap();
		assert_eq!(target(Layout::Room, &k), Target::Room(rb));
		// The watch prefixes themselves (watch.rs) must land on the same targets.
		let k = serialize_key((&u, Interfix)).unwrap();
		assert_eq!(target(Layout::User, &k), Target::User(ub));
		let k = serialize_key((&r, Interfix)).unwrap();
		assert_eq!(target(Layout::Room, &k), Target::Room(rb));
	}

	#[test]
	fn anything_unexpected_is_unknown_never_a_guess() {
		assert_eq!(target(Layout::User, b"!room:hs"), Target::Unknown);
		assert_eq!(target(Layout::User, b""), Target::Unknown);
		assert_eq!(target(Layout::SecondIsUser, b"@nosep:hs"), Target::Unknown);
		assert_eq!(target(Layout::SecondIsUser, &key(&[b"!r:hs", b"!r2:hs"])), Target::Unknown);
		assert_eq!(target(Layout::UserOrRoom, &key(&[b"#alias:hs", b"x"])), Target::Unknown);
		assert_eq!(target(Layout::Room, &key(&[b"@u:hs", b"x"])), Target::Unknown);
		assert_eq!(target(Layout::ShortRoom, &[1, 2, 3]), Target::Unknown);
	}

	#[test]
	fn covers_nothing_until_enabled_and_nothing_older_than_enable() {
		let idx = ChangeIndex::default();
		idx.record(Layout::User, b"@a:hs", 5);
		assert!(!idx.covers(100), "disabled");
		assert!(!idx.user_changed(b"@a:hs", 0), "writes before enable are not recorded");
		idx.enable(|| 50);
		assert!(!idx.covers(49), "a since from before enable must rebuild");
		assert!(idx.covers(50));
		assert!(idx.covers(1_000));
	}

	#[test]
	fn a_write_after_since_is_seen_and_one_at_or_before_is_not() {
		let idx = ChangeIndex::default();
		idx.enable(|| 10);
		idx.record(Layout::User, &key(&[b"@a:hs", b"!r:hs"]), 20);
		idx.record(Layout::Room, &key(&[b"!r:hs", b"c", b"@b:hs"]), 30);
		idx.record(Layout::ShortRoom, &9_u64.to_be_bytes(), 40);
		assert!(idx.user_changed(b"@a:hs", 19));
		assert!(!idx.user_changed(b"@a:hs", 20));
		assert!(!idx.user_changed(b"@nobody:hs", 0));
		assert!(idx.room_changed(b"!r:hs", None, 29));
		assert!(!idx.room_changed(b"!r:hs", None, 30));
		assert!(idx.room_changed(b"!other:hs", Some(9), 39));
		assert!(!idx.room_changed(b"!other:hs", Some(9), 40));
		// stamps only ever rise
		idx.record(Layout::User, b"@a:hs", 15);
		assert!(idx.user_changed(b"@a:hs", 19));
	}

	#[test]
	fn retiring_is_permanent() {
		let idx = ChangeIndex::default();
		idx.enable(|| 10);
		assert!(idx.covers(10));
		idx.retire();
		assert!(!idx.covers(10));
		assert!(!idx.covers(u64::MAX - 1));
		idx.enable(|| 10);
		assert!(!idx.covers(10), "a retired index never comes back");
		assert!(!idx.is_enabled());
	}

	#[test]
	fn an_unparseable_write_makes_every_older_since_rebuild() {
		let idx = ChangeIndex::default();
		idx.enable(|| 10);
		idx.record(Layout::User, b"garbage", 25);
		assert!(!idx.covers(24));
		assert!(idx.covers(25));
	}
}
