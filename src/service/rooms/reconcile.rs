//! Periodic MSC4521 reconciliation with joined room peers.

use std::{
	collections::{HashMap, HashSet},
	sync::{
		Arc, Mutex, PoisonError,
		atomic::{AtomicUsize, Ordering},
	},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::StreamExt;
use rand::seq::SliceRandom;
use rezzy_recon::{
	BucketDecodeBatch, BucketDecodeSuccess, ClientAction, ElementHash, EventIdFormat,
	MAX_BUCKETED_SKETCH_CAPACITY, MAX_LOCAL_SKETCH_DECODE_CAPACITY, MAX_RECONCILIATION_ROUNDS,
	ReconciliationClient, RemoteDigest, build_bucket_sketches,
};
use ruma::{
	OwnedEventId, OwnedRoomId, OwnedServerName, RoomId, RoomVersionId, ServerName,
	api::federation::{discovery::get_server_version, event::get_event},
};
use tokio::{sync::Notify, time::MissedTickBehavior};
use tuwunel_core::{Result, Server};

use crate::{
	service::{Args, Service as ServiceTrait},
	services::OnceServices,
};

pub mod api {
	use std::collections::BTreeMap;

	use ruma::{
		OwnedEventId, OwnedRoomId,
		api::{
			auth_scheme::NoAuthentication, federation::authentication::ServerSignatures,
			metadata, request, response,
		},
	};

	pub mod version {
		use super::*;
		metadata! { method: GET, rate_limited: false, authentication: NoAuthentication,
		path: "/_matrix/federation/v1/version", }
		#[request]
		pub struct Request {}
		#[response]
		pub struct Response {
			pub server: Option<ruma::api::federation::discovery::get_server_version::v1::Server>,
			#[serde(default)]
			pub unstable_features: Option<BTreeMap<String, bool>>,
		}
	}

	pub mod digest {
		use super::*;
		metadata! { method: GET, rate_limited: false, authentication: ServerSignatures,
		history: { unstable => "/_matrix/federation/unstable/tk.nutra.msc0501/room_digest/{room_id}", } }
		#[request]
		pub struct Request {
			#[ruma_api(path)]
			pub room_id: OwnedRoomId,
		}
		#[response]
		pub struct Response {
			pub digest: String,
			pub digest_type: String,
			pub known_event_count: u64,
			pub frame_id: String,
			pub extremity_event_ids: Vec<OwnedEventId>,
			pub frame_event_ids: Vec<OwnedEventId>,
			pub depth_range: Vec<u64>,
			pub origin_server_ts_range: Vec<u64>,
		}
	}
	pub mod strata {
		use super::*;
		metadata! { method: GET, rate_limited: false, authentication: ServerSignatures,
		history: { unstable => "/_matrix/federation/unstable/tk.nutra.msc0501/room_digest/{room_id}/strata", } }
		#[request]
		pub struct Request {
			#[ruma_api(path)]
			pub room_id: OwnedRoomId,
		}
		#[response]
		pub struct Response {
			pub digest: String,
			pub digest_type: String,
			pub known_event_count: u64,
			pub frame_id: String,
			pub extremity_event_ids: Vec<OwnedEventId>,
			pub frame_event_ids: Vec<OwnedEventId>,
			pub depth_range: Vec<u64>,
			pub origin_server_ts_range: Vec<u64>,
			pub strata: Vec<String>,
		}
	}
	pub mod diff {
		use super::*;
		#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
		pub struct SketchBucket {
			pub depth: u64,
			pub prefix: String,
			pub capacity: u64,
		}
		metadata! { method: POST, rate_limited: false, authentication: ServerSignatures,
		history: { unstable => "/_matrix/federation/unstable/tk.nutra.msc0501/room_diff/{room_id}", } }
		#[request]
		pub struct Request {
			#[ruma_api(path)]
			pub room_id: OwnedRoomId,
			pub mode: String,
			pub digest_type: String,
			pub local_digest: String,
			pub local_known_event_count: u64,
			pub frame_id: String,
			pub frame_event_ids: Vec<OwnedEventId>,
			#[serde(default)]
			pub frame_negotiation: bool,
			pub scope: Option<String>,
			pub state_at: Option<OwnedEventId>,
			pub requests: Vec<SketchBucket>,
			pub local_sketches: Vec<String>,
		}
		#[response]
		pub struct Response {
			pub digest_type: String,
			pub missing_event_ids: Vec<OwnedEventId>,
			pub requester_only_short_ids: Option<Vec<String>>,
			pub expected_requester_side_accumulator: Option<String>,
			pub remote_known_event_count: u64,
			pub remote_extremity_event_ids: Vec<OwnedEventId>,
			pub frame_id: String,
			pub frame_status: String,
			pub negotiated_frame_event_ids: Option<Vec<OwnedEventId>>,
			pub scope: Option<String>,
			pub state_at: Option<OwnedEventId>,
			pub sketch_status: String,
			pub truncated: bool,
		}
	}
}

const DIGEST_INTERVAL: Duration = Duration::from_secs(2);
const PEER_COOLDOWN: Duration = Duration::from_secs(2);
const MAX_ROOMS_PER_SWEEP: usize = 8;
const MAX_PENDING_ROOMS: usize = 16_384;
const MAX_PEERS_PER_ROOM: usize = 4;
const MAX_EVENT_FETCHES: usize = 100;
const REMEMBER_FOR: Duration = Duration::from_secs(3600);
const MAX_REMEMBERED_EVENTS: usize = 20_000;
const MIN_EXCHANGE_SPACING: Duration = Duration::from_millis(250);
const SAMPLE_FETCHES: usize = 16;
const MAX_SAMPLE_FAILURE_PERCENT: usize = 25;
const DIGEST_FEATURE: &str = "tk.nutra.msc4521.digest.algebraic_v1.0.0";
const BINDING_FEATURE: &str = "tk.nutra.msc4521.binding.event_id_v1";
const STRATA_FEATURE: &str = "tk.nutra.msc0501.strata";
const UNSUPPORTED_COOLDOWN: Duration = Duration::from_secs(24 * 3600);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	mutex
		.lock()
		.unwrap_or_else(PoisonError::into_inner)
}

pub struct Reconcile {
	server: Arc<Server>,
	services: Arc<OnceServices>,
	last_done: Mutex<HashMap<(OwnedRoomId, OwnedServerName), Instant>>,
	unusable: Mutex<HashMap<(OwnedServerName, OwnedEventId), Instant>>,
	room_cursor: AtomicUsize,
	wake: Notify,
	pending_rooms: Mutex<HashSet<OwnedRoomId>>,
}

#[async_trait]
impl ServiceTrait for Reconcile {
	fn build(args: &Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			server: args.server.clone(),
			services: args.services.clone(),
			last_done: Default::default(),
			unusable: Default::default(),
			room_cursor: AtomicUsize::new(0),
			wake: Notify::new(),
			pending_rooms: Default::default(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		let mut tick = tokio::time::interval(DIGEST_INTERVAL);
		tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
		let mut last_exchange = None;
		loop {
			tokio::select! {
				() = self.server.until_shutdown() => return Ok(()),
				_ = tick.tick() => {},
				_ = self.wake.notified() => {},
			}
			if !self.server.config.allow_federation || !self.server.config.msc4521_enabled {
				continue;
			}
			let now = Instant::now();
			lock(&self.last_done).retain(|_, until| *until > now);
			let ours = self.services.globals.server_name().to_owned();
			let mut rooms: Vec<OwnedRoomId> = self
				.services
				.state_cache
				.server_rooms(&ours)
				.map(ToOwned::to_owned)
				.collect()
				.await;
			if !rooms.is_empty() {
				let offset = self
					.room_cursor
					.fetch_add(MAX_ROOMS_PER_SWEEP, Ordering::Relaxed)
					% rooms.len();
				rooms.rotate_left(offset);
			}
			let mut selected: Vec<OwnedRoomId> = {
				let mut pending = lock(&self.pending_rooms);
				let urgent: Vec<_> = pending
					.iter()
					.take(MAX_ROOMS_PER_SWEEP)
					.cloned()
					.collect();
				for room in &urgent {
					pending.remove(room);
				}
				urgent
			};
			for room in rooms {
				if selected.len() >= MAX_ROOMS_PER_SWEEP {
					break;
				}
				if !selected.contains(&room) {
					selected.push(room);
				}
			}
			for room in selected {
				let mut peers: Vec<OwnedServerName> = self
					.services
					.state_cache
					.room_servers(&room)
					.map(ToOwned::to_owned)
					.collect()
					.await;
				peers.shuffle(&mut rand::rng());
				for peer in peers.into_iter().take(MAX_PEERS_PER_ROOM) {
					if peer == ours {
						continue;
					}
					let key = (room.clone(), peer.to_owned());
					if lock(&self.last_done)
						.get(&key)
						.is_some_and(|until| *until > Instant::now())
					{
						continue;
					}
					let spacing = last_exchange
						.map(|last: Instant| MIN_EXCHANGE_SPACING.saturating_sub(last.elapsed()))
						.unwrap_or(Duration::ZERO);
					if !spacing.is_zero() {
						tokio::time::sleep(spacing).await;
					}
					last_exchange = Some(Instant::now());
					if let Ok(supported) = self.exchange(&room, &peer).await {
						lock(&self.last_done).insert(
							key,
							Instant::now()
								+ if supported {
									PEER_COOLDOWN
								} else {
									UNSUPPORTED_COOLDOWN
								},
						);
					}
				}
			}
		}
	}

	fn name(&self) -> &str { crate::service::make_name(module_path!()) }
}

impl Reconcile {
	/// Drops the room's synchronized cooldown and schedules an early sweep.
	pub fn room_activity(&self, room_id: &RoomId) {
		lock(&self.last_done).retain(|(room, _), _| room != room_id);
		let mut pending = lock(&self.pending_rooms);
		if pending.len() < MAX_PENDING_ROOMS {
			pending.insert(room_id.to_owned());
		}
		drop(pending);
		self.wake.notify_one();
	}

	async fn exchange(&self, room: &RoomId, peer: &ServerName) -> Result<bool> {
		if !self
			.services
			.state_cache
			.server_in_room(self.services.globals.server_name(), room)
			.await
			|| !self
				.services
				.state_cache
				.server_in_room(peer, room)
				.await
			|| self
				.services
				.event_handler
				.acl_check(peer, room)
				.await
				.is_err()
		{
			return Ok(true);
		}
		// MSC4521: the event_ids binding MUST NOT be used in room versions 1 and 2.
		if matches!(
			self.services.state.get_room_version(room).await?,
			RoomVersionId::V1 | RoomVersionId::V2
		) {
			return Ok(false);
		}
		let local = self
			.services
			.timeline
			.reconciliation_state(room)
			.await?;
		let version = self
			.services
			.federation
			.execute(peer, api::version::Request {})
			.await?;
		if !version
			.unstable_features
			.as_ref()
			.is_some_and(|features| {
				[DIGEST_FEATURE, BINDING_FEATURE, STRATA_FEATURE]
					.iter()
					.all(|name| features.get(*name) == Some(&true))
			})
		{
			return Ok(false);
		}
		let remote = match self
			.services
			.federation
			.execute(peer, api::strata::Request { room_id: room.to_owned() })
			.await
		{
			| Ok(response) => response,
			| Err(error) if error.is_not_found() => return Ok(false),
			| Err(error) => return Err(error),
		};
		if remote.digest_type != "algebraic_v1.0.0" {
			return Ok(false);
		}
		let remote_digest = RemoteDigest::from_wire(
			&remote.digest,
			remote.known_event_count,
			&remote.strata,
			&remote.frame_event_ids,
			&remote.frame_event_ids,
			&remote.extremity_event_ids,
			|_| true,
		)
		.map_err(|e| tuwunel_core::err!(Database("invalid reconciliation digest: {e:?}")))?;
		let client = ReconciliationClient::new(MAX_LOCAL_SKETCH_DECODE_CAPACITY)
			.map_err(|e| tuwunel_core::err!(Database("reconciliation client: {e:?}")))?;
		let mut action = client.select_action(&local.resident, remote_digest, 8);
		let mut offered = std::collections::BTreeSet::new();
		for _ in 0..MAX_RECONCILIATION_ROUNDS {
			match action {
				| ClientAction::Synchronized => break,
				| ClientAction::ExtremityDiff => break,
				| ClientAction::ResolveRoots { .. } => break,
				| ClientAction::BucketSketches { requests, accumulated_roots } => {
					let sketches = build_bucket_sketches(local.population.h64s(), &requests)
						.map_err(|e| {
							tuwunel_core::err!(Database("build reconciliation sketches: {e:?}"))
						})?;
					let response = self
						.services
						.federation
						.execute(peer, api::diff::Request {
							room_id: room.to_owned(),
							mode: "sketch".into(),
							digest_type: "algebraic_v1.0.0".into(),
							local_digest: local.resident.accumulator().encode_digest(),
							local_known_event_count: local
								.resident
								.accumulator()
								.known_event_count(),
							frame_id: remote.frame_id.clone(),
							frame_event_ids: remote.frame_event_ids.clone(),
							frame_negotiation: false,
							scope: Some("event_set".into()),
							state_at: None,
							requests: requests
								.iter()
								.map(|r| api::diff::SketchBucket {
									depth: u64::from(r.depth),
									prefix: r.prefix.to_string(),
									capacity: r.capacity as u64,
								})
								.collect(),
							local_sketches: sketches.iter().map(|s| s.encode()).collect(),
						})
						.await?;
					if response.sketch_status == "capacity_exceeded" {
						let batch = BucketDecodeBatch {
							successful_buckets: vec![],
							failed_buckets: requests
								.iter()
								.map(|r| (r.depth, r.prefix))
								.collect(),
						};
						action = ReconciliationClient::transition_bucket_batch(
							batch,
							&requests,
							accumulated_roots,
							None,
							MAX_BUCKETED_SKETCH_CAPACITY,
						);
						continue;
					}
					if response.sketch_status != "decoded" {
						return Err(tuwunel_core::err!(BadServerResponse(
							"peer returned an unsupported sketch status"
						)));
					}
					let mut roots = Vec::new();
					for id in &response.missing_event_ids {
						if let Some(hash) = short_hash(id) {
							roots.push(hash);
							offered.insert(id.clone());
						}
					}
					if let Some(ids) = &response.requester_only_short_ids {
						for id in ids {
							if let Ok(bytes) = URL_SAFE_NO_PAD.decode(id) {
								if bytes.len() == 8 {
									roots.push(u64::from_be_bytes(
										bytes.try_into().expect("checked length"),
									));
								}
							}
						}
					}
					let successes = requests
						.iter()
						.map(|r| BucketDecodeSuccess {
							depth: r.depth,
							prefix: r.prefix,
							roots: roots
								.iter()
								.copied()
								.filter(|h| bucket_prefix(*h, r.depth) == r.prefix)
								.collect(),
						})
						.collect();
					action = ReconciliationClient::transition_bucket_batch(
						BucketDecodeBatch {
							successful_buckets: successes,
							failed_buckets: vec![],
						},
						&requests,
						accumulated_roots,
						None,
						MAX_BUCKETED_SKETCH_CAPACITY,
					);
				},
			}
		}
		self.fetch_offered(room, peer, offered).await;
		Ok(true)
	}

	async fn fetch_offered(
		&self,
		room: &RoomId,
		peer: &ServerName,
		offered: std::collections::BTreeSet<OwnedEventId>,
	) {
		let mut event_ids = Vec::new();
		for event_id in offered {
			let key = (peer.to_owned(), event_id.clone());
			let unusable = lock(&self.unusable)
				.get(&key)
				.is_some_and(|until| *until > Instant::now());
			if unusable
				|| self
					.services
					.timeline
					.non_outlier_pdu_exists(&event_id)
					.await
					.is_ok()
			{
				continue;
			}
			event_ids.push(event_id);
		}
		event_ids.shuffle(&mut rand::rng());
		event_ids.truncate(MAX_EVENT_FETCHES);
		let sample_count = event_ids.len().min(SAMPLE_FETCHES);
		let mut failed_sample = 0;
		let mut fetched = Vec::new();
		for (index, event_id) in event_ids.into_iter().enumerate() {
			let key = (peer.to_owned(), event_id.clone());
			let response = self
				.services
				.federation
				.execute(peer, get_event::v1::Request { event_id: event_id.clone() })
				.await;
			let response = match response {
				| Ok(response) => response,
				| Err(error) => {
					if error.is_not_found() {
						self.remember_unusable(key);
					}
					if index < sample_count {
						failed_sample += 1;
					}
					if index + 1 >= sample_count
						&& failed_sample * 100 > sample_count * MAX_SAMPLE_FAILURE_PERCENT
					{
						break;
					}
					continue;
				},
			};
			let Ok(value) = serde_json::from_str::<serde_json::Value>(response.pdu.get()) else {
				self.remember_unusable(key);
				if index < sample_count {
					failed_sample += 1;
				}
				if index + 1 >= sample_count
					&& failed_sample * 100 > sample_count * MAX_SAMPLE_FAILURE_PERCENT
				{
					break;
				}
				continue;
			};
			let depth = value
				.get("depth")
				.and_then(|v| v.as_u64())
				.unwrap_or(u64::MAX);
			let Ok(raw) = serde_json::value::RawValue::from_string(response.pdu.get().to_owned())
			else {
				self.remember_unusable(key);
				if index < sample_count {
					failed_sample += 1;
				}
				if index + 1 >= sample_count
					&& failed_sample * 100 > sample_count * MAX_SAMPLE_FAILURE_PERCENT
				{
					break;
				}
				continue;
			};
			let Ok((room_id, parsed_id, pdu)) = self
				.services
				.event_handler
				.parse_incoming_pdu(&raw)
				.await
			else {
				self.remember_unusable(key);
				if index < sample_count {
					failed_sample += 1;
				}
				if index + 1 >= sample_count
					&& failed_sample * 100 > sample_count * MAX_SAMPLE_FAILURE_PERCENT
				{
					break;
				}
				continue;
			};
			if room_id == room && parsed_id == event_id {
				fetched.push((depth, event_id, pdu));
			} else {
				self.remember_unusable(key);
				if index < sample_count {
					failed_sample += 1;
				}
			}
			if index + 1 >= sample_count
				&& failed_sample * 100 > sample_count * MAX_SAMPLE_FAILURE_PERCENT
			{
				break;
			}
		}
		fetched.sort_by_key(|(depth, ..)| *depth);
		for (_, event_id, pdu) in fetched {
			let result = self
				.services
				.event_handler
				.handle_incoming_pdu(peer, room, &event_id, pdu, true)
				.await;
			if result.is_err() {
				self.remember_unusable((peer.to_owned(), event_id));
			}
		}
	}

	fn remember_unusable(&self, key: (OwnedServerName, OwnedEventId)) {
		let now = Instant::now();
		let mut unusable = lock(&self.unusable);
		unusable.retain(|_, until| *until > now);
		if unusable.len() < MAX_REMEMBERED_EVENTS {
			unusable.insert(key, now + REMEMBER_FOR);
		}
	}
}

fn short_hash(id: &OwnedEventId) -> Option<u64> {
	[EventIdFormat::V4Plus, EventIdFormat::V3]
		.into_iter()
		.find_map(|format| ElementHash::from_matrix_event_id(id.as_str(), format).ok())
		.map(|hash| hash.h64)
}
fn bucket_prefix(hash: u64, depth: u8) -> u64 {
	if depth == 0 { 0 } else { hash >> (64 - u32::from(depth)) }
}
