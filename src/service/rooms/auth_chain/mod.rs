mod data;

use std::{
	collections::{BTreeSet, HashSet, VecDeque},
	fmt::Debug,
	sync::Arc,
	time::Instant,
};

use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{EventId, OwnedEventId, RoomId};
use tuwunel_core::{
	Err, Result, at, debug, debug_error, implement, trace,
	utils::{
		IterStream,
		stream::{ReadyExt, TryBroadbandExt},
	},
	validated, warn,
};

use self::data::Data;
use crate::{Dep, rooms, rooms::short::ShortEventId};

pub struct Service {
	services: Services,
	db: Data,
}

struct Services {
	short: Dep<rooms::short::Service>,
	timeline: Dep<rooms::timeline::Service>,
}

type Bucket<'a> = BTreeSet<(u64, &'a EventId)>;

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: Services {
				short: args.depend::<rooms::short::Service>("rooms::short"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
			},
			db: Data::new(&args),
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

#[implement(Service)]
pub fn event_ids_iter<'a, I>(
	&'a self,
	room_id: &'a RoomId,
	starting_events: I,
) -> impl Stream<Item = Result<OwnedEventId>> + Send + 'a
where
	I: Iterator<Item = &'a EventId> + Clone + Debug + ExactSizeIterator + Send + 'a,
{
	self.get_auth_chain(room_id, starting_events)
		.map_ok(|chain| {
			self.services
				.short
				.multi_get_eventid_from_short(chain.into_iter().stream())
				.ready_filter(Result::is_ok)
		})
		.try_flatten_stream()
}

#[implement(Service)]
#[tracing::instrument(name = "auth_chain", level = "debug", skip_all)]
pub async fn get_auth_chain<'a, I>(
	&'a self,
	room_id: &RoomId,
	starting_events: I,
) -> Result<Vec<ShortEventId>>
where
	I: Iterator<Item = &'a EventId> + Clone + Debug + ExactSizeIterator + Send + 'a,
{
	const NUM_BUCKETS: usize = 50; //TODO: change possible w/o disrupting db?
	const BUCKET: Bucket<'_> = BTreeSet::new();

	let started = Instant::now();
	let mut starting_ids = self
		.services
		.short
		.multi_get_or_create_shorteventid(starting_events.clone())
		.zip(starting_events.clone().stream())
		.boxed();

	let mut buckets = [BUCKET; NUM_BUCKETS];
	while let Some((short, starting_event)) = starting_ids.next().await {
		let bucket: usize = short.try_into()?;
		let bucket: usize = validated!(bucket % NUM_BUCKETS);
		buckets[bucket].insert((short, starting_event));
	}

	debug!(
		starting_events = ?starting_events.count(),
		elapsed = ?started.elapsed(),
		"start",
	);

	let full_auth_chain: Vec<ShortEventId> = buckets
		.into_iter()
		.try_stream()
		.broad_and_then(|chunk| self.get_auth_chain_outer(room_id, started, chunk))
		.try_collect()
		.map_ok(|auth_chain: Vec<_>| auth_chain.into_iter().flatten().collect())
		.map_ok(|mut full_auth_chain: Vec<_>| {
			full_auth_chain.sort_unstable();
			full_auth_chain.dedup();
			full_auth_chain
		})
		.boxed()
		.await?;

	debug!(
		chain_length = ?full_auth_chain.len(),
		elapsed = ?started.elapsed(),
		"done",
	);

	Ok(full_auth_chain)
}

#[implement(Service)]
async fn get_auth_chain_outer(
	&self,
	room_id: &RoomId,
	started: Instant,
	chunk: Bucket<'_>,
) -> Result<Vec<ShortEventId>> {
	let chunk_key: Vec<ShortEventId> = chunk.iter().map(at!(0)).collect();

	if chunk_key.is_empty() {
		return Ok(Vec::new());
	}

	if let Ok(cached) = self
		.get_cached_eventid_authchain(&chunk_key)
		.await
	{
		return Ok(cached.to_vec());
	}

	let chunk_cache: Vec<_> = chunk
		.into_iter()
		.try_stream()
		.broad_and_then(|(shortid, event_id)| async move {
			if let Ok(cached) = self
				.get_cached_eventid_authchain(&[shortid])
				.await
			{
				return Ok(cached.to_vec());
			}

			let auth_chain = self
				.get_auth_chain_inner(room_id, event_id)
				.await?;
			self.cache_auth_chain_vec(vec![shortid], auth_chain.as_slice());
			debug!(
				?event_id,
				elapsed = ?started.elapsed(),
				"Cache missed event"
			);

			Ok(auth_chain)
		})
		.try_collect()
		.map_ok(|chunk_cache: Vec<_>| chunk_cache.into_iter().flatten().collect())
		.map_ok(|mut chunk_cache: Vec<_>| {
			chunk_cache.sort_unstable();
			chunk_cache.dedup();
			chunk_cache
		})
		.await?;

	self.cache_auth_chain_vec(chunk_key, chunk_cache.as_slice());
	debug!(
		chunk_cache_length = ?chunk_cache.len(),
		elapsed = ?started.elapsed(),
		"Cache missed chunk",
	);

	Ok(chunk_cache)
}

#[implement(Service)]
#[tracing::instrument(name = "inner", level = "trace", skip(self, room_id))]
async fn get_auth_chain_inner(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
) -> Result<Vec<ShortEventId>> {
	let mut todo: VecDeque<_> = [event_id.to_owned()].into();
	let mut found = HashSet::new();

	while let Some(event_id) = todo.pop_front() {
		trace!(?event_id, "processing auth event");

		match self.services.timeline.get_pdu(&event_id).await {
			| Err(e) => {
				debug_error!(?event_id, ?e, "Could not find pdu mentioned in auth events");
			},
			| Ok(pdu) => {
				if pdu.room_id != room_id {
					return Err!(Request(Forbidden(error!(
						?event_id,
						?room_id,
						wrong_room_id = ?pdu.room_id,
						"auth event for incorrect room"
					))));
				}

				for auth_event in &pdu.auth_events {
					let sauthevent = self
						.services
						.short
						.get_or_create_shorteventid(auth_event)
						.await;

					if found.insert(sauthevent) {
						trace!(?event_id, ?auth_event, "adding auth event to processing queue");

						todo.push_back(auth_event.clone());
					}
				}
			},
		}
	}

	Ok(found.into_iter().collect())
}

#[implement(Service)]
#[inline]
pub async fn get_cached_eventid_authchain(&self, key: &[u64]) -> Result<Arc<[ShortEventId]>> {
	self.db.get_cached_eventid_authchain(key).await
}

#[implement(Service)]
#[tracing::instrument(skip_all, level = "debug")]
pub fn cache_auth_chain(&self, key: Vec<u64>, auth_chain: &HashSet<ShortEventId>) {
	let val: Arc<[ShortEventId]> = auth_chain.iter().copied().collect();

	self.db.cache_auth_chain(key, val);
}

#[implement(Service)]
#[tracing::instrument(skip_all, level = "debug")]
pub fn cache_auth_chain_vec(&self, key: Vec<u64>, auth_chain: &[ShortEventId]) {
	let val: Arc<[ShortEventId]> = auth_chain.iter().copied().collect();

	self.db.cache_auth_chain(key, val);
}

#[implement(Service)]
pub fn get_cache_usage(&self) -> (usize, usize) {
	let cache = self.db.auth_chain_cache.lock().expect("locked");

	(cache.len(), cache.capacity())
}

#[implement(Service)]
pub fn clear_cache(&self) {
	self.db
		.auth_chain_cache
		.lock()
		.expect("locked")
		.clear();
}
