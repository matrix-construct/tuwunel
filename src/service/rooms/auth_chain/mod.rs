use std::{
	collections::{BTreeSet, HashSet},
	iter::once,
	sync::Arc,
	time::Instant,
};

use async_trait::async_trait;
use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, pin_mut,
	stream::{FuturesUnordered, unfold},
};
use ruma::{
	EventId, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId,
	room_version_rules::RoomVersionRules,
};
use serde::Deserialize;
use tuwunel_core::{
	Err, Result, at, debug, debug_error, err, implement,
	itertools::Itertools,
	matrix::room_version,
	pdu::AuthEvents,
	smallvec::SmallVec,
	trace, utils,
	utils::{
		IterStream,
		stream::{BroadbandExt, ReadyExt, TryExpect, automatic_width},
	},
	validated, warn,
};
use tuwunel_database::Map;

use crate::rooms::short::ShortEventId;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	authchainkey_authchain: Arc<Map>,
	shorteventid_authchain: Arc<Map>,
}

type Bucket<'a> = BTreeSet<(ShortEventId, &'a EventId)>;
type CacheKey = SmallVec<[ShortEventId; 1]>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data {
				authchainkey_authchain: args.db["authchainkey_authchain"].clone(),
				shorteventid_authchain: args.db["shorteventid_authchain"].clone(),
			},
		}))
	}

	async fn clear_cache(&self) { self.db.authchainkey_authchain.clear().await; }

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
pub fn event_ids_iter<'a, I>(
	&'a self,
	room_id: &'a RoomId,
	room_version: &'a RoomVersionId,
	starting_events: I,
) -> impl Stream<Item = Result<OwnedEventId>> + Send + 'a
where
	I: Iterator<Item = &'a EventId> + Clone + ExactSizeIterator + Send + 'a,
{
	self.get_auth_chain(room_id, room_version, starting_events)
		.map_ok(|chain| {
			self.services
				.short
				.multi_get_eventid_from_short(chain.into_iter().stream())
				.ready_filter(Result::is_ok)
		})
		.try_flatten_stream()
}

#[implement(Service)]
#[tracing::instrument(
	name = "auth_chain",
	level = "debug",
	skip_all,
	fields(
		%room_id,
		starting_events = %starting_events.clone().count(),
	)
)]
pub async fn get_auth_chain<'a, I>(
	&'a self,
	room_id: &RoomId,
	room_version: &RoomVersionId,
	starting_events: I,
) -> Result<Vec<ShortEventId>>
where
	I: Iterator<Item = &'a EventId> + Clone + ExactSizeIterator + Send + 'a,
{
	const NUM_BUCKETS: usize = 50; //TODO: change possible w/o disrupting db?
	const BUCKET: Bucket<'_> = BTreeSet::new();

	let started = Instant::now();
	let room_rules = room_version::rules(room_version)?;
	let starting_events_count = starting_events.clone().count();
	let starting_ids = self
		.services
		.short
		.multi_get_or_create_shorteventid(starting_events.clone())
		.zip(starting_events.stream());

	pin_mut!(starting_ids);
	let mut buckets = [BUCKET; NUM_BUCKETS];
	while let Some((short, starting_event)) = starting_ids.next().await {
		let bucket: usize = short.try_into()?;
		let bucket: usize = validated!(bucket % NUM_BUCKETS);
		buckets[bucket].insert((short, starting_event));
	}

	debug!(
		starting_events = starting_events_count,
		elapsed = ?started.elapsed(),
		"start",
	);

	let full_auth_chain: Vec<ShortEventId> = buckets
		.iter()
		.stream()
		.flat_map_unordered(automatic_width(), |starting_events| {
			self.get_chunk_auth_chain(
				room_id,
				&started,
				starting_events.iter().copied(),
				&room_rules,
			)
			.boxed()
		})
		.collect::<Vec<_>>()
		.map(IntoIterator::into_iter)
		.map(Itertools::sorted_unstable)
		.map(Itertools::dedup)
		.map(Iterator::collect)
		.boxed()
		.await;

	debug!(
		chain_length = ?full_auth_chain.len(),
		elapsed = ?started.elapsed(),
		"done",
	);

	Ok(full_auth_chain)
}

#[implement(Service)]
#[tracing::instrument(
	name = "chain_outer",
	level = "debug",
	skip_all,
	fields(
		starting_events = %starting_events.clone().count(),
	)
)]
pub fn get_chunk_auth_chain<'a, I>(
	&'a self,
	room_id: &'a RoomId,
	started: &'a Instant,
	starting_events: I,
	room_rules: &'a RoomVersionRules,
) -> impl Stream<Item = ShortEventId> + Send + 'a
where
	I: Iterator<Item = (ShortEventId, &'a EventId)> + Clone + Send + Sync + 'a,
{
	let starting_shortids = starting_events.clone().map(at!(0));

	let build_chain = async |(shortid, event_id): (ShortEventId, &'a EventId)| {
		if let Ok(cached) = self.get_cached_auth_chain(once(shortid)).await {
			return cached;
		}

		let auth_chain: Vec<_> = self
			.get_event_auth_chain(room_id, event_id, room_rules)
			.collect()
			.await;

		self.put_cached_auth_chain(once(shortid), auth_chain.as_slice());
		debug!(
			?event_id,
			elapsed = ?started.elapsed(),
			"Cache missed event"
		);

		auth_chain
	};

	let cache_chain = move |chunk_cache: &Vec<_>| {
		self.put_cached_auth_chain(starting_shortids, chunk_cache.as_slice());
		debug!(
			chunk_cache_length = ?chunk_cache.len(),
			elapsed = ?started.elapsed(),
			"Cache missed chunk",
		);
	};

	self.get_cached_auth_chain(starting_events.clone().map(at!(0)))
		.map_ok(IntoIterator::into_iter)
		.map_ok(IterStream::try_stream)
		.or_else(move |_| async move {
			starting_events
				.clone()
				.stream()
				.broad_then(build_chain)
				.collect::<Vec<_>>()
				.map(IntoIterator::into_iter)
				.map(Iterator::flatten)
				.map(Itertools::sorted_unstable)
				.map(Itertools::dedup)
				.map(Iterator::collect)
				.inspect(cache_chain)
				.map(IntoIterator::into_iter)
				.map(IterStream::try_stream)
				.map(Ok)
				.await
		})
		.try_flatten_stream()
		.map_expect("either cache hit or cache miss yields a chain")
}

#[implement(Service)]
#[tracing::instrument(name = "chain_inner", level = "trace", skip_all)]
pub fn get_event_auth_chain<'a>(
	&'a self,
	room_id: &'a RoomId,
	event_id: &'a EventId,
	room_rules: &'a RoomVersionRules,
) -> impl Stream<Item = ShortEventId> + Send + 'a {
	self.get_event_auth_chain_ids(room_id, event_id, room_rules)
		.broad_then(async move |auth_event| {
			self.services
				.short
				.get_or_create_shorteventid(&auth_event)
				.await
		})
}

#[implement(Service)]
#[tracing::instrument(
	name = "inner_ids",
	level = "debug",
	skip_all,
	fields(%event_id)
)]
pub fn get_event_auth_chain_ids<'a>(
	&'a self,
	room_id: &'a RoomId,
	event_id: &'a EventId,
	room_rules: &'a RoomVersionRules,
) -> impl Stream<Item = OwnedEventId> + Send + 'a {
	struct State<Fut> {
		todo: FuturesUnordered<Fut>,
		seen: HashSet<OwnedEventId>,
	}

	let starting_events = self.get_event_auth_event_ids(room_id, event_id.to_owned());

	let state = State {
		todo: once(starting_events).collect(),
		seen: room_rules
			.authorization
			.room_create_event_id_as_room_id
			.then_some(room_id.as_event_id().ok())
			.into_iter()
			.flatten()
			.collect(),
	};

	unfold(state, move |mut state| async move {
		match state.todo.next().await {
			| None => None,
			| Some(Err(_)) => Some((AuthEvents::new().into_iter().stream(), state)),
			| Some(Ok(auth_events)) => {
				let push = |auth_event: &OwnedEventId| {
					trace!(?event_id, ?auth_event, "push");
					state
						.todo
						.push(self.get_event_auth_event_ids(room_id, auth_event.clone()));
				};

				let seen = |auth_event: OwnedEventId| {
					state
						.seen
						.insert(auth_event.clone())
						.then_some(auth_event)
				};

				let out = auth_events
					.into_iter()
					.filter_map(seen)
					.inspect(push)
					.collect::<AuthEvents>()
					.into_iter()
					.stream();

				Some((out, state))
			},
		}
	})
	.flatten()
}

#[implement(Service)]
#[tracing::instrument(
	name = "cache_put",
	level = "debug",
	skip_all,
	fields(
		key_len = key.clone().count(),
		chain_len = auth_chain.len(),
	)
)]
fn put_cached_auth_chain<I>(&self, key: I, auth_chain: &[ShortEventId])
where
	I: Iterator<Item = ShortEventId> + Clone + Send,
{
	let key = key.collect::<CacheKey>();

	debug_assert!(!key.is_empty(), "auth_chain key must not be empty");

	self.db
		.authchainkey_authchain
		.put(key.as_slice(), auth_chain);

	if key.len() == 1 {
		self.db
			.shorteventid_authchain
			.put(key, auth_chain);
	}
}

#[implement(Service)]
#[tracing::instrument(
	name = "cache_get",
	level = "trace",
	skip_all,
	fields(
		key_len = %key.clone().count()
	),
)]
async fn get_cached_auth_chain<I>(&self, key: I) -> Result<Vec<ShortEventId>>
where
	I: Iterator<Item = ShortEventId> + Clone + Send,
{
	let key = key.collect::<CacheKey>();

	if key.is_empty() {
		return Ok(Vec::new());
	}

	// Check cache. On miss, check first-order table for single-event keys.
	let chain = self
		.db
		.authchainkey_authchain
		.qry(key.as_slice())
		.map_err(|_| err!(Request(NotFound("auth_chain not cached"))))
		.or_else(async |e| {
			if key.len() > 1 {
				return Err(e);
			}

			self.db
				.shorteventid_authchain
				.qry(&key[0])
				.map_err(|_| err!(Request(NotFound("auth_chain not found"))))
				.await
		})
		.await?
		.chunks_exact(size_of::<u64>())
		.map(utils::u64_from_u8)
		.collect();

	Ok(chain)
}

#[implement(Service)]
#[tracing::instrument(
	name = "auth_events",
	level = "trace",
	ret(level = "trace")
	skip_all,
	fields(%event_id)
)]
async fn get_event_auth_event_ids<'a>(
	&'a self,
	room_id: &'a RoomId,
	event_id: OwnedEventId,
) -> Result<AuthEvents> {
	#[derive(Deserialize)]
	struct Pdu {
		auth_events: AuthEvents,
		room_id: OwnedRoomId,
	}

	let pdu: Pdu = self
		.services
		.timeline
		.get(&event_id)
		.inspect_err(|e| {
			debug_error!(?event_id, ?room_id, "auth chain event: {e}");
		})
		.await?;

	if pdu.room_id != room_id {
		return Err!(Request(Forbidden(error!(
			?event_id,
			?room_id,
			wrong_room_id = ?pdu.room_id,
			"auth event for incorrect room",
		))));
	}

	Ok(pdu.auth_events)
}
