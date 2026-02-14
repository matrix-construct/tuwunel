mod acl_check;
mod fetch_auth;
mod fetch_prev;
mod fetch_state;
mod handle_incoming_pdu;
mod handle_outlier_pdu;
mod handle_prev_pdu;
mod parse_incoming_pdu;
mod resolve_state;
mod state_at_incoming;
mod upgrade_outlier_pdu;

use std::{
	collections::{HashMap, hash_map},
	fmt::Write,
	ops::Range,
	sync::{Arc, RwLock},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId};
use tuwunel_core::{
	Err, Result, implement,
	matrix::{Event, PduEvent},
	utils::{MutexMap, bytes::pretty, continue_exponential_backoff},
};

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;

type RateLimitState = (Instant, u32); // Time if last failed try, number of failed tries

pub struct Service {
	pub mutex_federation: RoomMutexMap,
	services: Arc<crate::services::OnceServices>,
	bad_event_ratelimiter: Arc<RwLock<HashMap<OwnedEventId, RateLimitState>>>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			mutex_federation: RoomMutexMap::new(),
			services: args.services.clone(),
			bad_event_ratelimiter: Arc::new(RwLock::new(HashMap::new())),
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex_federation = self.mutex_federation.len();
		writeln!(out, "federation_mutex: {mutex_federation}")?;

		let (ber_count, ber_bytes) = self.bad_event_ratelimiter.read()?.iter().fold(
			(0_usize, 0_usize),
			|(mut count, mut bytes), (event_id, _)| {
				bytes = bytes.saturating_add(event_id.capacity());
				bytes = bytes.saturating_add(size_of::<RateLimitState>());
				count = count.saturating_add(1);
				(count, bytes)
			},
		);

		writeln!(out, "bad_event_ratelimiter: {ber_count} ({})", pretty(ber_bytes))?;

		Ok(())
	}

	async fn clear_cache(&self) {
		self.bad_event_ratelimiter
			.write()
			.expect("locked for writing")
			.clear();
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
fn back_off(&self, event_id: &EventId) {
	use hash_map::Entry::{Occupied, Vacant};

	match self
		.bad_event_ratelimiter
		.write()
		.expect("locked")
		.entry(event_id.into())
	{
		| Vacant(e) => {
			e.insert((Instant::now(), 1));
		},
		| Occupied(mut e) => {
			*e.get_mut() = (Instant::now(), e.get().1.saturating_add(1));
		},
	}
}

#[implement(Service)]
fn is_backed_off(&self, event_id: &EventId, range: Range<Duration>) -> bool {
	let Some((time, tries)) = self
		.bad_event_ratelimiter
		.read()
		.expect("locked")
		.get(event_id)
		.copied()
	else {
		return false;
	};

	continue_exponential_backoff(range.start, range.end, time.elapsed(), tries)
}

#[implement(Service)]
#[tracing::instrument(
	name = "exists",
	level = "trace",
	ret(level = "trace"),
	skip_all,
	fields(%event_id)
)]
async fn event_exists(&self, event_id: &EventId) -> bool {
	self.services.timeline.pdu_exists(event_id).await
}

#[implement(Service)]
#[tracing::instrument(
	name = "fetch",
	level = "trace",
	skip_all,
	fields(%event_id)
)]
async fn event_fetch(&self, event_id: &EventId) -> Result<PduEvent> {
	self.services.timeline.get_pdu(event_id).await
}

fn check_room_id<Pdu: Event>(room_id: &RoomId, pdu: &Pdu) -> Result {
	if pdu.room_id() != room_id {
		return Err!(Request(InvalidParam(error!(
			pdu_event_id = ?pdu.event_id(),
			pdu_room_id = ?pdu.room_id(),
			?room_id,
			"Found event from room in room",
		))));
	}

	Ok(())
}
