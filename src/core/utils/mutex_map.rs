//! Per-key asynchronous mutual exclusion with automatic entry cleanup.
//!
//! Each key maps to a Tokio mutex shared by its current contenders. The last
//! contender to release its claim removes the entry, whether it held the mutex
//! or was canceled while waiting for it.

use std::{
	fmt::Debug,
	hash::Hash,
	sync::{Arc, TryLockError::WouldBlock},
};

use tokio::sync::OwnedMutexGuard as Omg;

use crate::{Result, err};

/// Provides independent asynchronous mutexes keyed by owned values.
///
/// Lock acquisition creates entries on demand, and callers contending for the
/// same key serialize. An entry lives exactly as long as some caller holds or
/// contends for it.
#[derive(Debug)]
pub struct MutexMap<Key, Val> {
	map: Map<Key, Val>,
}

/// Keeps a keyed mutex locked until the guard is dropped.
///
/// The guard retains the parent map so cleanup remains possible. Dropping it
/// releases the keyed mutex and then removes the entry when no other holder or
/// contender references it.
#[derive(Debug)]
#[clippy::has_significant_drop]
pub struct Guard<Key, Val> {
	map: Map<Key, Val>,
	entry: Option<Value<Val>>,
	val: Option<Omg<Val>>,
}

type Map<Key, Val> = Arc<MapMutex<Key, Val>>;
type MapMutex<Key, Val> = std::sync::Mutex<HashMap<Key, Val>>;
type HashMap<Key, Val> = std::collections::HashMap<Key, Value<Val>>;
type Value<Val> = Arc<tokio::sync::Mutex<Val>>;

impl<Key, Val> MutexMap<Key, Val>
where
	Key: Clone + Eq + Hash + Send,
	Val: Default + Send,
{
	/// Creates an empty keyed mutex map.
	///
	/// No per-key mutex is allocated until a lock method first sees its key.
	/// The result is equivalent to [`Default::default`].
	#[must_use]
	pub fn new() -> Self {
		Self {
			map: Map::new(MapMutex::new(HashMap::new())),
		}
	}

	/// Acquires the asynchronous mutex associated with a key.
	///
	/// The method creates an entry if absent and waits for the current holder
	/// to release it. Cancellation while waiting releases the claim on the
	/// entry, and a poisoned internal map mutex causes a panic.
	#[tracing::instrument(level = "trace", skip(self))]
	pub async fn lock<K>(&self, k: &K) -> Guard<Key, Val>
	where
		K: Debug + Send + ?Sized + Sync + ToOwned<Owned = Key>,
	{
		self.entry(k).lock().await
	}

	/// Attempts to acquire a key without waiting for its asynchronous mutex.
	///
	/// The key entry is created if absent, and contention returns an error
	/// instead of yielding. Acquiring the internal map mutex can still block
	/// and panics if that mutex is poisoned.
	#[tracing::instrument(level = "trace", skip(self))]
	pub fn try_lock<K>(&self, k: &K) -> Result<Guard<Key, Val>>
	where
		K: Debug + Send + ?Sized + Sync + ToOwned<Owned = Key>,
	{
		self.entry(k).try_lock()
	}

	/// Attempts to acquire a key without yielding, blocking only to release a
	/// failed attempt.
	///
	/// Contention on either the internal map or keyed mutex returns an error.
	/// The entry is created only after the map mutex is acquired, and releasing
	/// a failed attempt blocks on that mutex again. A poisoned map mutex causes
	/// a panic.
	#[tracing::instrument(level = "trace", skip(self))]
	pub fn try_try_lock<K>(&self, k: &K) -> Result<Guard<Key, Val>>
	where
		K: Debug + Send + ?Sized + Sync + ToOwned<Owned = Key>,
	{
		self.try_entry(k)?.try_lock()
	}

	/// Reports whether the map currently contains an entry for a key.
	///
	/// An entry represents a held mutex or contenders that still reference it.
	/// The check locks the internal map and panics if that mutex is poisoned.
	#[must_use]
	pub fn contains(&self, k: &Key) -> bool { self.map.lock().expect("locked").contains_key(k) }

	/// Reports whether no keyed mutex entries are currently tracked.
	///
	/// A false result implies at least one active holder or contender. The
	/// check locks the internal map and panics if that mutex is poisoned.
	#[must_use]
	pub fn is_empty(&self) -> bool { self.map.lock().expect("locked").is_empty() }

	/// Returns the number of keyed mutex entries currently tracked.
	///
	/// The count includes held mutexes and entries retained by contenders. The
	/// check locks the internal map and panics if that mutex is poisoned.
	#[must_use]
	pub fn len(&self) -> usize { self.map.lock().expect("locked").len() }

	/// Returns a copy of the keys currently tracked.
	///
	/// Each key has a held mutex or contenders still referencing it, and the
	/// two are not told apart. The copy is taken under the internal map lock,
	/// which panics if poisoned.
	#[must_use]
	pub fn keys(&self) -> impl ExactSizeIterator<Item = Key> + Send + use<Key, Val> {
		self.map
			.lock()
			.expect("locked")
			.keys()
			.cloned()
			.collect::<Vec<_>>()
			.into_iter()
	}

	fn entry<K>(&self, k: &K) -> Guard<Key, Val>
	where
		K: ?Sized + ToOwned<Owned = Key>,
	{
		let val = self
			.map
			.lock()
			.expect("locked")
			.entry(k.to_owned())
			.or_default()
			.clone();

		self.pending(val)
	}

	fn try_entry<K>(&self, k: &K) -> Result<Guard<Key, Val>>
	where
		K: ?Sized + ToOwned<Owned = Key>,
	{
		let val = self
			.map
			.try_lock()
			.map_err(|e| match e {
				| WouldBlock => err!("would block"),
				| _ => panic!("{e:?}"),
			})?
			.entry(k.to_owned())
			.or_default()
			.clone();

		Ok(self.pending(val))
	}

	fn pending(&self, val: Value<Val>) -> Guard<Key, Val> {
		Guard {
			map: Arc::clone(&self.map),
			entry: Some(val),
			val: None,
		}
	}
}

impl<Key, Val> Default for MutexMap<Key, Val>
where
	Key: Clone + Eq + Hash + Send,
	Val: Default + Send,
{
	fn default() -> Self { Self::new() }
}

impl<Key, Val> Guard<Key, Val> {
	async fn lock(mut self) -> Self {
		// The in-flight claim must release before this guard, so a cancellation
		// leaves the entry unreferenced.
		let val = self.claim();

		self.val = Some(val.lock_owned().await);
		self
	}

	fn try_lock(mut self) -> Result<Self> {
		self.val = self
			.claim()
			.try_lock_owned()
			.map_err(|_| err!("would yield"))
			.map(Some)?;

		Ok(self)
	}

	fn claim(&self) -> Value<Val> { Arc::clone(self.entry.as_ref().expect("claimed")) }
}

impl<Key, Val> Drop for Guard<Key, Val> {
	#[tracing::instrument(name = "unlock", level = "trace", skip_all)]
	fn drop(&mut self) {
		self.val.take();

		// Releasing the claim under the map lock elects the last one out.
		let mut map = self.map.lock().expect("locked");

		if self
			.entry
			.take()
			.is_some_and(|val| Arc::strong_count(&val) <= 2)
		{
			map.retain(|_, val| Arc::strong_count(val) > 1);
		}
	}
}
