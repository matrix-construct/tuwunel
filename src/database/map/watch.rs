use std::{
	collections::{BTreeMap, btree_map::Entry},
	future::Future,
	ops::RangeToInclusive,
	sync::Mutex,
};

use futures::pin_mut;
use serde::Serialize;
use tokio::sync::watch::{Sender, channel};
use tuwunel_core::{debug, implement, smallvec::SmallVec};

use crate::keyval::{KeyBuf, serialize_key};

type Watchers = Mutex<BTreeMap<KeyBuf, Sender<()>>>;
type KeyVec = SmallVec<[KeyBuf; 1]>;

#[derive(Default)]
pub(super) struct Watch {
	watchers: Watchers,
}

#[implement(super::Map)]
pub fn watch_prefix<K>(&self, prefix: K) -> impl Future<Output = ()> + Send + '_
where
	K: Serialize,
{
	let prefix = serialize_key(prefix).expect("failed to serialize watch prefix key");
	self.watch_raw_prefix(&prefix)
}

#[implement(super::Map)]
pub fn watch_raw_prefix<'a, K>(&self, prefix: &'a K) -> impl Future<Output = ()> + Send + use<K>
where
	K: AsRef<[u8]> + ?Sized + 'a,
{
	let rx = match self
		.watch
		.watchers
		.lock()
		.expect("locked")
		.entry(prefix.as_ref().into())
	{
		| Entry::Occupied(node) => node.get().subscribe(),
		| Entry::Vacant(node) => {
			let (tx, rx) = channel(());
			node.insert(tx);
			rx
		},
	};

	async move {
		pin_mut!(rx);
		rx.changed()
			.await
			.expect("watcher sender dropped");
	}
}

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
	// Before the watchers: whoever they wake may consult what the hook recorded.
	if let Some(hook) = self.change_hook.get() {
		hook(key.as_ref());
	}

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
