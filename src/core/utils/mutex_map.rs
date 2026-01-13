use std::{
	borrow::ToOwned,
	fmt::Debug,
	hash::Hash,
	sync::{Arc, TryLockError::WouldBlock},
};

use tokio::sync::OwnedMutexGuard as Omg;

use crate::{Result, err};

/// Map of Mutexes
#[derive(Debug)]
pub struct MutexMap<Key, Val> {
	map: Map<Key, Val>,
}

#[derive(Debug)]
pub struct Guard<Key, Val> {
	map: Map<Key, Val>,
	val: Omg<Val>,
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
	#[must_use]
	pub fn new() -> Self {
		Self {
			map: Map::new(MapMutex::new(HashMap::new())),
		}
	}

	#[tracing::instrument(level = "trace", skip(self))]
	pub async fn lock<K>(&self, k: &K) -> Guard<Key, Val>
	where
		K: Debug + Send + ?Sized + Sync + ToOwned<Owned = Key>,
	{
		let val = self
			.map
			.lock()
			.expect("locked")
			.entry(k.to_owned())
			.or_default()
			.clone();

		Guard::<Key, Val> {
			map: Arc::clone(&self.map),
			val: val.lock_owned().await,
		}
	}

	#[tracing::instrument(level = "trace", skip(self))]
	pub fn try_lock<K>(&self, k: &K) -> Result<Guard<Key, Val>>
	where
		K: Debug + Send + ?Sized + Sync + ToOwned<Owned = Key>,
	{
		let val = self
			.map
			.lock()
			.expect("locked")
			.entry(k.to_owned())
			.or_default()
			.clone();

		Ok(Guard::<Key, Val> {
			map: Arc::clone(&self.map),
			val: val
				.try_lock_owned()
				.map_err(|_| err!("would yield"))?,
		})
	}

	#[tracing::instrument(level = "trace", skip(self))]
	pub fn try_try_lock<K>(&self, k: &K) -> Result<Guard<Key, Val>>
	where
		K: Debug + Send + ?Sized + Sync + ToOwned<Owned = Key>,
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

		Ok(Guard::<Key, Val> {
			map: Arc::clone(&self.map),
			val: val
				.try_lock_owned()
				.map_err(|_| err!("would yield"))?,
		})
	}

	#[must_use]
	pub fn contains(&self, k: &Key) -> bool { self.map.lock().expect("locked").contains_key(k) }

	#[must_use]
	pub fn is_empty(&self) -> bool { self.map.lock().expect("locked").is_empty() }

	#[must_use]
	pub fn len(&self) -> usize { self.map.lock().expect("locked").len() }
}

impl<Key, Val> Default for MutexMap<Key, Val>
where
	Key: Clone + Eq + Hash + Send,
	Val: Default + Send,
{
	fn default() -> Self { Self::new() }
}

impl<Key, Val> Drop for Guard<Key, Val> {
	#[tracing::instrument(name = "unlock", level = "trace", skip_all)]
	fn drop(&mut self) {
		if Arc::strong_count(Omg::mutex(&self.val)) <= 2 {
			self.map.lock().expect("locked").retain(|_, val| {
				!Arc::ptr_eq(val, Omg::mutex(&self.val)) || Arc::strong_count(val) > 2
			});
		}
	}
}
