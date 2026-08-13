use std::{
	collections::{BTreeMap, btree_map::Entry},
	ops::RangeToInclusive,
	sync::Mutex,
};

use futures::pin_mut;
use serde::Serialize;
use tokio::sync::watch::{Receiver, Sender, channel};
use tuwunel_core::{debug, defer, implement, smallvec::SmallVec};

use crate::keyval::{KeyBuf, serialize_key};

/// Stores prefix subscriptions in raw-key order.
///
/// Ordered storage lets notification walk reverse prefix candidates for a
/// changed key. The first nonmatching candidate terminates that walk.
type Watchers = Mutex<BTreeMap<KeyBuf, Sender<()>>>;
/// Buffers stale watcher keys discovered during notification.
///
/// The one-entry inline budget avoids allocation when notification reaps no
/// more than one closed subscription. Larger reap batches spill to the heap.
type KeyVec = SmallVec<[KeyBuf; 1]>;

/// Owns the prefix subscriptions registered for a map.
///
/// A mutex protects subscription insertion, notification, and stale-entry
/// removal.
#[derive(Default)]
pub(super) struct Watch {
	watchers: Watchers,
}

/// Waits for the next map mutation under a serialized prefix.
///
/// The prefix is encoded once before subscription. The stored subscription is
/// reaped after a later matching notification observes that its receiver has
/// closed.
///
/// # Panics
///
/// Panics if prefix serialization fails, the watcher mutex is poisoned, or the
/// sender disappears before notification.
#[implement(super::Map)]
pub fn watch_prefix<K>(&self, prefix: K) -> impl Future<Output = ()> + Send + '_
where
	K: Serialize,
{
	let prefix = serialize_key(prefix).expect("failed to serialize watch prefix key");
	self.watch_raw_prefix(&prefix)
}

/// Waits once for a map mutation under a raw prefix.
///
/// The prefix is copied into the subscription table. A drop guard removes the
/// entry immediately when this future owns its last receiver.
///
/// # Panics
///
/// Panics if the watcher mutex is poisoned or the sender disappears before
/// notification.
#[implement(super::Map)]
pub fn watch_raw_prefix_once<K>(&self, prefix: K) -> impl Future<Output = ()> + Send + '_
where
	K: AsRef<[u8]>,
{
	let key: KeyBuf = prefix.as_ref().into();
	let rx = self.subscribe(key.clone());

	async move {
		pin_mut!(rx);

		// We are still subscribed, so a receiver count of one means we are the last.
		defer! {{
			let mut watchers = self.watch.watchers.lock().expect("locked");
			if watchers.get(&key).is_some_and(|tx| tx.receiver_count() == 1) {
				watchers.remove(&key);
			}
		}}

		rx.changed()
			.await
			.expect("watcher sender dropped");
	}
}

/// Waits for the next map mutation under a borrowed raw prefix.
///
/// The prefix is copied into the subscription table. The stored subscription is
/// reaped after a later matching notification observes that its receiver has
/// closed.
///
/// # Panics
///
/// Panics if the watcher mutex is poisoned or the sender disappears before
/// notification.
#[implement(super::Map)]
pub fn watch_raw_prefix<'a, K>(&self, prefix: &'a K) -> impl Future<Output = ()> + Send + use<K>
where
	K: AsRef<[u8]> + ?Sized + 'a,
{
	let rx = self.subscribe(prefix.as_ref().into());

	async move {
		pin_mut!(rx);
		rx.changed()
			.await
			.expect("watcher sender dropped");
	}
}

/// Subscribes to mutations under an owned raw prefix.
///
/// Existing prefixes share one watch sender, while new prefixes create a fresh
/// channel.
///
/// # Panics
///
/// Panics if the watcher mutex is poisoned.
#[implement(super::Map)]
fn subscribe(&self, key: KeyBuf) -> Receiver<()> {
	match self
		.watch
		.watchers
		.lock()
		.expect("locked")
		.entry(key)
	{
		| Entry::Occupied(node) => node.get().subscribe(),
		| Entry::Vacant(node) => {
			let (tx, rx) = channel(());
			node.insert(tx);
			rx
		},
	}
}

/// Notifies subscriptions whose prefixes match a mutated raw key.
///
/// Closed subscriptions discovered during the ordered prefix walk are removed
/// in the same critical section. Live subscriptions remain available for later
/// mutations.
///
/// # Panics
///
/// Panics if the watcher mutex is poisoned.
#[implement(super::Map)]
#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(
		map = self.name(),
		key = str::from_utf8(key.as_ref()).unwrap_or("<binary>"),
	)
)]
pub(crate) fn notify<K>(&self, key: &K)
where
	K: AsRef<[u8]> + Ord + ?Sized,
{
	let range = RangeToInclusive::<KeyBuf> { end: key.as_ref().into() };

	let mut watchers = self.watch.watchers.lock().expect("locked");

	let num_notified = watchers
		.range(range)
		.rev()
		.take_while(|(k, _)| key.as_ref().starts_with(k))
		.filter_map(|(k, tx)| tx.send(()).is_err().then_some(k))
		.cloned()
		.collect::<KeyVec>()
		.into_iter()
		.fold(0_usize, |num_notified, key| {
			watchers.remove(&key);
			num_notified.saturating_add(1)
		});

	if num_notified > 0 {
		debug!(watchers = watchers.len(), num_notified, "notified");
	}
}

#[cfg(test)]
mod tests {
	use tokio::sync::watch::channel;

	// Pins the tokio contract the reaper relies on: receiver_count() reflects a
	// just-dropped Receiver and send() fails once no receiver remains.
	#[test]
	fn receiver_count_reaps_at_last_drop() {
		let (tx, rx) = channel(());
		assert_eq!(tx.receiver_count(), 1, "fresh channel has one receiver");

		let rx2 = tx.subscribe();
		assert_eq!(tx.receiver_count(), 2, "subscribe adds a receiver");

		drop(rx2);
		assert_eq!(tx.receiver_count(), 1, "drop is reflected synchronously");

		drop(rx);
		assert_eq!(tx.receiver_count(), 0, "last drop leaves no receiver");
		assert!(tx.send(()).is_err(), "send fails with zero receivers");
	}
}
