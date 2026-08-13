//! Last-delivered push badge counts.
//!
//! Remembers, per user and pushkey, the unread total the push gateway last
//! accepted. Intentionally in-memory only: an absent entry forces the next
//! counts-only refresh to send, so a restart reconciles every pusher with a
//! badge this server has never observed. Reaping on pusher delete,
//! replacement, and device removal bounds the map near the live pusher
//! population.

use std::{
	collections::{BTreeMap, HashMap},
	sync::{Mutex, MutexGuard, PoisonError},
};

use ruma::{OwnedUserId, UInt, UserId};
use tuwunel_core::implement;

type Badges = HashMap<OwnedUserId, BTreeMap<String, UInt>>;

#[derive(Default)]
pub(super) struct SentBadges {
	inner: Mutex<Badges>,
}

/// Return the unread total last accepted by this pusher's gateway.
///
/// `None` means no delivery has been confirmed since startup, and the caller
/// must send rather than assume agreement.
#[implement(super::Service)]
pub(super) fn sent_badge(&self, user_id: &UserId, pushkey: &str) -> Option<UInt> {
	self.sent_badges
		.lock()
		.get(user_id)
		.and_then(|pushkeys| pushkeys.get(pushkey))
		.copied()
}

impl SentBadges {
	fn lock(&self) -> MutexGuard<'_, Badges> {
		self.inner
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
	}
}

/// Record the unread total a pusher's gateway just accepted.
///
/// Call only after a successful delivery; recording an attempt would make a
/// failed send look reconciled and suppress the retry's refresh.
#[implement(super::Service)]
pub(super) fn record_sent_badge(&self, user_id: &UserId, pushkey: &str, unread: UInt) {
	self.sent_badges
		.lock()
		.entry(user_id.to_owned())
		.or_default()
		.insert(pushkey.to_owned(), unread);
}

/// Forget the delivery record for one pusher.
///
/// A deleted or replaced pusher leaves the device state unknown, so the next
/// refresh must send unconditionally.
#[implement(super::Service)]
pub(super) fn forget_sent_badge(&self, user_id: &UserId, pushkey: &str) {
	let mut badges = self.sent_badges.lock();
	let Some(pushkeys) = badges.get_mut(user_id) else {
		return;
	};

	pushkeys.remove(pushkey);

	if pushkeys.is_empty() {
		badges.remove(user_id);
	}
}
