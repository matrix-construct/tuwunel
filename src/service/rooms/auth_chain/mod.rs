use std::{
	collections::{BTreeSet, HashSet},
	fmt::Debug,
	iter::once,
	sync::Arc,
	time::Instant,
};

use async_trait::async_trait;
use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, pin_mut,
	stream::{FuturesUnordered, unfold},
};
use ruma::{EventId, OwnedEventId, RoomId, RoomVersionId, room_version_rules::RoomVersionRules};
use tuwunel_core::{
	Err, Result, at, debug, debug_error, err, implement,
	matrix::{Event, PduEvent, room_version},
	pdu::AuthEvents,
	trace, utils,
	utils::{
		IterStream,
		stream::{BroadbandExt, ReadyExt, TryBroadbandExt},
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

type Bucket<'a> = BTreeSet<(u64, &'a EventId)>;

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
	I: Iterator<Item = &'a EventId> + Clone + Debug + ExactSizeIterator + Send + 'a,
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
	fields(%room_id),
)]
pub async fn get_auth_chain<'a, I>(
	&'a self,
	room_id: &RoomId,
	room_version: &RoomVersionId,
	starting_events: I,
) -> Result<Vec<ShortEventId>>
where
	I: Iterator<Item = &'a EventId> + Clone + Debug + ExactSizeIterator + Send + 'a,
{
	const NUM_BUCKETS: usize = 50; //TODO: change possible w/o disrupting db?
	const BUCKET: Bucket<'_> = BTreeSet::new();

	let started = Instant::now();
	let room_rules = room_version::rules(room_version)?;
	let starting_ids = self
		.services
		.short
		.multi_get_or_create_shorteventid(starting_events.clone())
		.zip(starting_events.clone().stream());

	pin_mut!(starting_ids);
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
		.broad_and_then(|chunk| self.get_auth_chain_outer(room_id, started, chunk, &room_rules))
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
async fn get_auth_chain_outer<'a>(
	&'a self,
	room_id: &'a RoomId,
	started: Instant,
	chunk: Bucket<'_>,
	room_rules: &RoomVersionRules,
) -> Result<Vec<ShortEventId>> {
	let chunk_key: Vec<ShortEventId> = chunk.iter().map(at!(0)).collect();

	if let Ok(cached) = self.get_cached_auth_chain(&chunk_key).await {
		return Ok(cached);
	}

	let chunk_cache = chunk
		.into_iter()
		.stream()
		.broad_then(async |(shortid, event_id)| {
			if let Ok(cached) = self.get_cached_auth_chain(&[shortid]).await {
				return cached;
			}

			let auth_chain: Vec<_> = self
				.get_auth_chain_inner(room_id, event_id, room_rules)
				.collect()
				.await;

			self.put_cached_auth_chain(&[shortid], auth_chain.as_slice());
			debug!(
				?event_id,
				elapsed = ?started.elapsed(),
				"Cache missed event"
			);

			auth_chain
		})
		.collect()
		.map(|chunk_cache: Vec<_>| chunk_cache.into_iter().flatten().collect())
		.map(|mut chunk_cache: Vec<_>| {
			chunk_cache.sort_unstable();
			chunk_cache.dedup();
			chunk_cache
		})
		.await;

	self.put_cached_auth_chain(&chunk_key, chunk_cache.as_slice());
	debug!(
		chunk_cache_length = ?chunk_cache.len(),
		elapsed = ?started.elapsed(),
		"Cache missed chunk",
	);

	Ok(chunk_cache)
}

#[implement(Service)]
#[tracing::instrument(
	name = "inner",
	level = "trace",
	skip_all,
	fields(%event_id),
)]
fn get_auth_chain_inner<'a>(
	&'a self,
	room_id: &'a RoomId,
	event_id: &'a EventId,
	room_rules: &'a RoomVersionRules,
) -> impl Stream<Item = ShortEventId> + Send + 'a {
	struct State<Fut> {
		todo: FuturesUnordered<Fut>,
		seen: HashSet<OwnedEventId>,
	}

	let state = State {
		todo: once(self.get_pdu(room_id, event_id.to_owned())).collect(),
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
			| Some(Ok(pdu)) => {
				let push = |auth_event: &OwnedEventId| {
					trace!(?event_id, ?auth_event, "push");
					state
						.todo
						.push(self.get_pdu(room_id, auth_event.clone()));
				};

				let seen = |auth_event: OwnedEventId| {
					state
						.seen
						.insert(auth_event.clone())
						.then_some(auth_event)
				};

				let out = pdu
					.auth_events()
					.map(ToOwned::to_owned)
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
	.broad_then(async move |auth_event| {
		self.services
			.short
			.get_or_create_shorteventid(&auth_event)
			.await
	})
}

#[implement(Service)]
async fn get_pdu<'a>(&'a self, room_id: &'a RoomId, event_id: OwnedEventId) -> Result<PduEvent> {
	let pdu = self
		.services
		.timeline
		.get_pdu(&event_id)
		.await
		.inspect_err(|e| {
			debug_error!(?event_id, ?room_id, "auth chain event: {e}");
		})?;

	if pdu.room_id() != room_id {
		return Err!(Request(Forbidden(error!(
			?event_id,
			?room_id,
			wrong_room_id = ?pdu.room_id(),
			"auth event for incorrect room",
		))));
	}

	Ok(pdu)
}

#[implement(Service)]
#[tracing::instrument(skip_all, level = "debug")]
fn put_cached_auth_chain(&self, key: &[ShortEventId], auth_chain: &[ShortEventId]) {
	debug_assert!(!key.is_empty(), "auth_chain key must not be empty");

	self.db
		.authchainkey_authchain
		.put(key, auth_chain);

	if key.len() == 1 {
		self.db
			.shorteventid_authchain
			.put(key, auth_chain);
	}
}

#[implement(Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn get_cached_auth_chain(&self, key: &[u64]) -> Result<Vec<ShortEventId>> {
	if key.is_empty() {
		return Ok(Vec::new());
	}

	// Check cache. On miss, check first-order table for single-event keys.
	let chain = self
		.db
		.authchainkey_authchain
		.qry(key)
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
		.await?;

	let chain = chain
		.chunks_exact(size_of::<u64>())
		.map(utils::u64_from_u8)
		.collect();

	Ok(chain)
}
