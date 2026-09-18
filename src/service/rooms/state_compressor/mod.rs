//! Encodes room state snapshots as compact parent-linked deltas.
//!
//! Each state entry combines a short state key with a short event ID in a
//! fixed-width record. Reconstructed parent chains are cached to make repeated
//! state resolution inexpensive while bounding persistent diff depth.

use std::{
	collections::{BTreeSet, HashMap},
	fmt::{Debug, Write},
	sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use lru_cache::LruCache;
use ruma::{EventId, RoomId};
use tuwunel_core::{
	Result,
	arrayvec::ArrayVec,
	at, checked, err, expected, implement, utils,
	utils::{bytes, math::usize_from_f64, stream::IterStream},
};
use tuwunel_database::{Map, Txn};

use crate::rooms::short::{ShortEventId, ShortId, ShortStateHash, ShortStateKey};

/// Persists, reconstructs, and caches compressed room state snapshots.
///
/// New snapshots are stored as bounded delta chains and flattened when their
/// depth or relative size becomes inefficient. Cached chain entries include
/// both each frame's delta and its fully materialized state.
pub struct Service {
	/// Reconstructed state chains keyed by their requested short state hash.
	pub stateinfo_cache: Mutex<StateInfoLruCache>,
	db: Data,
	services: Arc<crate::services::OnceServices>,
}

struct Data {
	shortstatehash_statediff: Arc<Map>,
}

/// One state as a delta against a parent state.
///
/// `added` and `removed` are the compressed entries this state adds to and
/// removes from its parent chain's accumulation; a `None` parent makes
/// `added` the full state.
#[derive(Clone)]
pub(crate) struct StateDiff {
	/// Parent snapshot against which this delta is applied, if any.
	pub(crate) parent: Option<ShortStateHash>,

	/// Compressed entries added to the parent snapshot.
	pub(crate) added: Arc<CompressedState>,

	/// Compressed entries removed from the parent snapshot.
	pub(crate) removed: Arc<CompressedState>,
}

/// Describes one materialized frame in a compressed state chain.
///
/// Frames are ordered from the root snapshot toward the requested snapshot.
/// Each frame retains its local delta alongside the resulting full state.
#[derive(Clone, Default)]
pub struct ShortStateInfo {
	/// Short hash identifying this state frame.
	pub shortstatehash: ShortStateHash,

	/// Fully materialized state after applying this frame.
	pub full_state: Arc<CompressedState>,

	/// Entries added by this frame relative to its parent.
	pub added: Arc<CompressedState>,

	/// Entries removed by this frame relative to its parent.
	pub removed: Arc<CompressedState>,
}

/// Reports a saved snapshot and its change from the room's previous state.
///
/// An unchanged snapshot reuses its short hash and returns empty added and
/// removed sets.
#[derive(Clone, Default)]
pub struct HashSetCompressStateEvent {
	/// Short hash identifying the saved snapshot.
	pub shortstatehash: ShortStateHash,

	/// Entries present only in the saved snapshot.
	pub added: Arc<CompressedState>,

	/// Entries present only in the previous snapshot.
	pub removed: Arc<CompressedState>,
}

type StateInfoLruCache = LruCache<ShortStateHash, ShortStateInfoVec>;
type ShortStateInfoVec = Vec<ShortStateInfo>;
type ParentStatesVec = Vec<ShortStateInfo>;

/// Ordered set of compressed state-key and event-ID pairs.
///
/// Ordering makes hashing, differences, and persistent serialization
/// deterministic for the same logical state.
pub type CompressedState = BTreeSet<CompressedStateEvent>;

/// Fixed-width encoding of one short state key and short event ID.
///
/// The first eight big-endian bytes hold the state key and the remaining eight
/// hold the event ID.
pub type CompressedStateEvent = [u8; 2 * size_of::<ShortId>()];

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;
		let cache_capacity =
			f64::from(config.stateinfo_cache_capacity) * config.cache_capacity_modifier;
		Ok(Arc::new(Self {
			stateinfo_cache: LruCache::new(usize_from_f64(cache_capacity)?).into(),
			db: Data {
				shortstatehash_statediff: args.db["shortstatehash_statediff"].clone(),
			},
			services: args.services.clone(),
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let (cache_len, ents) = {
			let cache = self.stateinfo_cache.lock().expect("locked");
			let ents = cache
				.iter()
				.map(at!(1))
				.flat_map(|vec| vec.iter())
				.fold(HashMap::new(), |mut ents, ssi| {
					for cs in &[&ssi.added, &ssi.removed, &ssi.full_state] {
						ents.insert(Arc::as_ptr(cs), compressed_state_size(cs));
					}

					ents
				});

			(cache.len(), ents)
		};

		let ents_len = ents.len();
		let bytes = ents
			.values()
			.copied()
			.fold(0_usize, usize::saturating_add);

		let bytes = bytes::pretty(bytes);
		writeln!(out, "- stateinfo_cache: {cache_len} entries, {ents_len} states ({bytes})")?;

		Ok(())
	}

	async fn clear_cache(&self) {
		self.stateinfo_cache
			.lock()
			.expect("locked")
			.clear();
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Loads and materializes the parent chain for a short state hash.
///
/// The returned frames are ordered root-first and include each frame's full
/// state plus its added and removed entries. A previously reconstructed chain
/// is returned from the LRU cache.
#[implement(Service)]
#[tracing::instrument(name = "load", level = "debug", skip(self))]
pub async fn load_shortstatehash_info(
	&self,
	shortstatehash: ShortStateHash,
) -> Result<ShortStateInfoVec> {
	if let Some(r) = self
		.stateinfo_cache
		.lock()?
		.get_mut(&shortstatehash)
	{
		return Ok(r.clone());
	}

	let stack = self
		.new_shortstatehash_info(shortstatehash)
		.await?;

	self.cache_shortstatehash_info(shortstatehash, stack.clone())
		.await?;

	Ok(stack)
}

/// Caches a reconstructed state chain under its requested short hash.
///
/// Lock poisoning is reported without modifying the cache.
#[implement(Service)]
#[tracing::instrument(
		name = "cache",
		level = "debug",
		skip_all,
		fields(
			?shortstatehash,
			stack = stack.len(),
		),
	)]
async fn cache_shortstatehash_info(
	&self,
	shortstatehash: ShortStateHash,
	stack: ShortStateInfoVec,
) -> Result {
	self.stateinfo_cache
		.lock()?
		.insert(shortstatehash, stack);

	Ok(())
}

#[implement(Service)]
async fn new_shortstatehash_info(
	&self,
	shortstatehash: ShortStateHash,
) -> Result<ShortStateInfoVec> {
	let StateDiff { parent, added, removed } = self.get_statediff(shortstatehash).await?;

	let Some(parent) = parent else {
		return Ok(vec![ShortStateInfo {
			shortstatehash,
			full_state: added.clone(),
			added,
			removed,
		}]);
	};

	let mut stack = Box::pin(self.load_shortstatehash_info(parent)).await?;
	let top = stack.last().expect("at least one frame");

	let mut full_state = (*top.full_state).clone();
	full_state.extend(added.iter().copied());

	let removed = (*removed).clone();
	for r in &removed {
		full_state.remove(r);
	}

	stack.push(ShortStateInfo {
		shortstatehash,
		added,
		removed: Arc::new(removed),
		full_state: Arc::new(full_state),
	});

	Ok(stack)
}

/// Compresses a stream of state-key and event-ID pairs.
///
/// Missing short event IDs are allocated as the returned stream is polled, and
/// each result packs both short IDs into the fixed-width representation.
#[implement(Service)]
pub fn compress_state_events<'a, I>(
	&'a self,
	state: I,
) -> impl Stream<Item = CompressedStateEvent> + Send + 'a
where
	I: Iterator<Item = (&'a ShortStateKey, &'a EventId)> + Clone + Debug + Send + 'a,
{
	let event_ids = state.clone().map(at!(1));

	let short_event_ids = self
		.services
		.short
		.multi_get_or_create_shorteventid(event_ids);

	state
		.stream()
		.map(at!(0))
		.zip(short_event_ids)
		.map(|(shortstatekey, shorteventid)| compress_state_event(*shortstatekey, shorteventid))
}

/// Compresses one state key and event ID into its fixed-width representation.
///
/// A short event ID is allocated first when the event has not been seen.
#[implement(Service)]
pub async fn compress_state_event(
	&self,
	shortstatekey: ShortStateKey,
	event_id: &EventId,
) -> CompressedStateEvent {
	let shorteventid = self
		.services
		.short
		.get_or_create_shorteventid(event_id)
		.await;

	compress_state_event(shortstatekey, shorteventid)
}

/// Stages a compressed state delta under a new short state hash.
///
/// The caller-owned transaction receives the row but is not executed here.
/// Chains deeper than three parent frames, or deltas too large relative to
/// their parent, are recursively flattened into an earlier layer.
#[implement(Service)]
pub fn save_state_from_diff(
	&self,
	txn: &mut Txn,
	shortstatehash: ShortStateHash,
	statediffnew: Arc<CompressedState>,
	statediffremoved: Arc<CompressedState>,
	diff_to_sibling: usize,
	mut parent_states: ParentStatesVec,
) -> Result {
	let statediffnew_len = statediffnew.len();
	let statediffremoved_len = statediffremoved.len();
	let diffsum = checked!(statediffnew_len + statediffremoved_len)?;

	if parent_states.len() > 3 {
		// Number of layers
		// To many layers, we have to go deeper
		let parent = parent_states
			.pop()
			.expect("parent must have a state");

		let mut parent_new = (*parent.added).clone();
		let mut parent_removed = (*parent.removed).clone();

		for removed in statediffremoved.iter() {
			if !parent_new.remove(removed) {
				// It was not added in the parent and we removed it
				parent_removed.insert(*removed);
			}
			// Else it was added in the parent and we removed it again. We
			// can forget this change
		}

		for new in statediffnew.iter() {
			if !parent_removed.remove(new) {
				// It was not touched in the parent and we added it
				parent_new.insert(*new);
			}
			// Else it was removed in the parent and we added it again. We
			// can forget this change
		}

		self.save_state_from_diff(
			txn,
			shortstatehash,
			Arc::new(parent_new),
			Arc::new(parent_removed),
			diffsum,
			parent_states,
		)?;

		return Ok(());
	}

	if parent_states.is_empty() {
		// There is no parent layer, create a new state
		self.save_statediff(txn, shortstatehash, &StateDiff {
			parent: None,
			added: statediffnew,
			removed: statediffremoved,
		});

		return Ok(());
	}

	// Else we have two options.
	// 1. We add the current diff on top of the parent layer.
	// 2. We replace a layer above

	let parent = parent_states
		.pop()
		.expect("parent must have a state");

	let parent_added_len = parent.added.len();
	let parent_removed_len = parent.removed.len();
	let parent_diff = checked!(parent_added_len + parent_removed_len)?;

	if checked!(diffsum * diffsum)? >= checked!(2 * diff_to_sibling * parent_diff)? {
		// Diff too big, we replace above layer(s)
		let mut parent_new = (*parent.added).clone();
		let mut parent_removed = (*parent.removed).clone();

		for removed in statediffremoved.iter() {
			if !parent_new.remove(removed) {
				// It was not added in the parent and we removed it
				parent_removed.insert(*removed);
			}
			// Else it was added in the parent and we removed it again. We
			// can forget this change
		}

		for new in statediffnew.iter() {
			if !parent_removed.remove(new) {
				// It was not touched in the parent and we added it
				parent_new.insert(*new);
			}
			// Else it was removed in the parent and we added it again. We
			// can forget this change
		}

		self.save_state_from_diff(
			txn,
			shortstatehash,
			Arc::new(parent_new),
			Arc::new(parent_removed),
			diffsum,
			parent_states,
		)?;
	} else {
		// Diff small enough, we add diff as layer on top of parent
		self.save_statediff(txn, shortstatehash, &StateDiff {
			parent: Some(parent.shortstatehash),
			added: statediffnew,
			removed: statediffremoved,
		});
	}

	Ok(())
}

/// Saves a complete compressed snapshot and reports its previous-state delta.
///
/// An existing content hash is reused; otherwise the short hash and delta row
/// are created together. Failure to reconstruct the previous chain is treated
/// as an absent parent, while an exactly unchanged snapshot returns empty sets.
#[implement(Service)]
#[tracing::instrument(skip(self, new_state_ids_compressed), level = "debug")]
pub async fn save_state(
	&self,
	room_id: &RoomId,
	new_state_ids_compressed: Arc<CompressedState>,
) -> Result<HashSetCompressStateEvent> {
	let previous_shortstatehash = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.await
		.ok();

	let state_hash = utils::calculate_hash(
		new_state_ids_compressed
			.iter()
			.map(|bytes| &bytes[..]),
	);

	let existing_shortstatehash = self
		.services
		.short
		.get_shortstatehash(&state_hash)
		.await
		.ok();

	if let Some(new_shortstatehash) = existing_shortstatehash
		.filter(|&new_shortstatehash| previous_shortstatehash.eq(&Some(new_shortstatehash)))
	{
		return Ok(HashSetCompressStateEvent {
			shortstatehash: new_shortstatehash,
			..Default::default()
		});
	}

	let states_parents = if let Some(p) = previous_shortstatehash {
		self.load_shortstatehash_info(p)
			.await
			.unwrap_or_default()
	} else {
		ShortStateInfoVec::new()
	};

	let (statediffnew, statediffremoved) = if let Some(parent_stateinfo) = states_parents.last() {
		let statediffnew: CompressedState = new_state_ids_compressed
			.difference(&parent_stateinfo.full_state)
			.copied()
			.collect();

		let statediffremoved: CompressedState = parent_stateinfo
			.full_state
			.difference(&new_state_ids_compressed)
			.copied()
			.collect();

		(Arc::new(statediffnew), Arc::new(statediffremoved))
	} else {
		(new_state_ids_compressed, Arc::new(CompressedState::new()))
	};

	let new_shortstatehash = if let Some(new_shortstatehash) = existing_shortstatehash {
		new_shortstatehash
	} else {
		self.services
			.short
			.get_or_create_shortstatehash(&state_hash, |txn, shortstatehash| {
				self.save_state_from_diff(
					txn,
					shortstatehash,
					statediffnew.clone(),
					statediffremoved.clone(),
					2, // every state change is 2 event changes on average
					states_parents,
				)
			})
			.await?
			.0
	};

	Ok(HashSetCompressStateEvent {
		shortstatehash: new_shortstatehash,
		added: statediffnew,
		removed: statediffremoved,
	})
}

/// Reads one state's delta row into its typed form.
///
/// Rows round-trip through [`save_statediff`], the pair being the only
/// codec for the statediff encoding.
///
/// # Panics
///
/// Panics if a stored delta row is shorter than its eight-byte parent prefix.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug", name = "get")]
pub(crate) async fn get_statediff(&self, shortstatehash: ShortStateHash) -> Result<StateDiff> {
	const BUFSIZE: usize = size_of::<ShortStateHash>();
	const STRIDE: usize = size_of::<ShortStateHash>();

	let value = self
		.db
		.shortstatehash_statediff
		.aqry::<BUFSIZE, _>(&shortstatehash)
		.await
		.map_err(|e| {
			err!(Database("Failed to find StateDiff from short {shortstatehash:?}: {e}"))
		})?;

	let parent = utils::u64_from_bytes(&value[0..size_of::<u64>()])
		.ok()
		.take_if(|parent| *parent != 0);

	debug_assert!(value.len().is_multiple_of(STRIDE), "value not aligned to stride");
	let _num_values = value.len() / STRIDE;

	let mut add_mode = true;
	let mut added = CompressedState::new();
	let mut removed = CompressedState::new();

	let mut i = STRIDE;
	while let Some(v) = value.get(i..expected!(i + 2 * STRIDE)) {
		if add_mode && v.starts_with(&0_u64.to_be_bytes()) {
			add_mode = false;
			i = expected!(i + STRIDE);
			continue;
		}
		if add_mode {
			added.insert(v.try_into()?);
		} else {
			removed.insert(v.try_into()?);
		}
		i = expected!(i + 2 * STRIDE);
	}

	Ok(StateDiff {
		parent,
		added: Arc::new(added),
		removed: Arc::new(removed),
	})
}

/// Serializes one state's delta into the caller's transaction.
///
/// Added and removed entries are emitted in sorted set order. The removed run
/// follows an all-zero sentinel only when it is nonempty, and this method does
/// not execute the transaction.
#[implement(Service)]
pub(crate) fn save_statediff(
	&self,
	txn: &mut Txn,
	shortstatehash: ShortStateHash,
	diff: &StateDiff,
) {
	let event_count = diff
		.added
		.len()
		.saturating_add(diff.removed.len());

	let event_bytes = event_count.saturating_mul(size_of::<CompressedStateEvent>());
	let separator_bytes =
		usize::from(!diff.removed.is_empty()).saturating_mul(size_of::<ShortStateHash>());

	let capacity = size_of::<ShortStateHash>()
		.saturating_add(event_bytes)
		.saturating_add(separator_bytes);

	let parent = diff.parent.unwrap_or(0_u64);
	let mut value = Vec::<u8>::with_capacity(capacity);
	value.extend_from_slice(&parent.to_be_bytes());

	for new in diff.added.iter() {
		value.extend_from_slice(&new[..]);
	}

	if !diff.removed.is_empty() {
		value.extend_from_slice(&0_u64.to_be_bytes());
		for removed in diff.removed.iter() {
			value.extend_from_slice(&removed[..]);
		}
	}

	txn.insert_raw(&self.db.shortstatehash_statediff, shortstatehash.to_be_bytes(), value);
}

/// Packs a short state key and short event ID into one compressed record.
///
/// Both IDs use big-endian encoding so byte ordering follows numeric ordering.
#[inline]
#[must_use]
pub(crate) fn compress_state_event(
	shortstatekey: ShortStateKey,
	shorteventid: ShortEventId,
) -> CompressedStateEvent {
	const SIZE: usize = size_of::<CompressedStateEvent>();

	let mut v = ArrayVec::<u8, SIZE>::new();
	v.extend(shortstatekey.to_be_bytes());
	v.extend(shorteventid.to_be_bytes());
	v.as_ref()
		.try_into()
		.expect("failed to create CompressedStateEvent")
}

/// Unpacks a compressed state record into its two short IDs.
///
/// This is the inverse of [`compress_state_event`].
#[inline]
#[must_use]
pub(crate) fn parse_compressed_state_event(
	compressed_event: CompressedStateEvent,
) -> (ShortStateKey, ShortEventId) {
	use utils::u64_from_u8;

	let shortstatekey = u64_from_u8(&compressed_event[0..size_of::<ShortStateKey>()]);
	let shorteventid = u64_from_u8(&compressed_event[size_of::<ShortStateKey>()..]);

	(shortstatekey, shorteventid)
}

#[inline]
fn compressed_state_size(compressed_state: &CompressedState) -> usize {
	compressed_state
		.len()
		.checked_mul(size_of::<CompressedStateEvent>())
		.expect("CompressedState size overflow")
}
