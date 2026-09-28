//! Incoming event handling, from the first signature check to the timeline.
//!
//! The service authorizes incoming PDUs, fetches the events they depend on,
//! derives their state and appends them. Observability counters cover the
//! local state build, the previous-event walk and the backoff verdicts. A
//! per-room history keeps the previous-event walk passes for up to three days.

mod acl_check;
mod backoff;
mod fetch_auth;
mod fetch_prev;
mod fetch_state;
mod handle_incoming_pdu;
mod handle_outlier_pdu;
mod handle_prev_pdu;
mod outlier_state;
mod parse_incoming_pdu;
mod policy_server;
mod prev_walk;
mod resolve_state;
mod state_at_incoming;
mod state_local_build;
mod upgrade_outlier_pdu;

use std::{fmt::Write, num::NonZeroUsize, sync::Arc};

use async_trait::async_trait;
use ruma::{EventId, OwnedRoomId, RoomVersionId, events::AnyStrippedStateEvent, serde::Raw};
use tuwunel_core::{Result, implement, matrix::PduEvent, utils::MutexMap};
use tuwunel_database::Map;

use self::{
	backoff::BackoffCounters,
	prev_walk::{InFlightWalks, PrevWalkCounters},
	state_local_build::StateLocalCounters,
};
pub use self::{
	backoff::{BackoffMetrics, Verdicts},
	policy_server::PolicyCheck,
	prev_walk::{
		InFlightWalk, Outcome as PrevWalkOutcome, PrevWalkMetrics, PrevWalkPass, PrevWalkRoom,
		Walk,
	},
	state_local_build::{LocalBuildReport, StateLocalMetrics},
};
use crate::service::make_name;

/// Handles incoming events: authorization, fetching missing events, state
/// resolution and upgrade into the timeline.
///
/// Federation transactions, joins, invites and backfill all route their PDUs
/// through it.
pub struct Service {
	/// Serializes room federation as the outermost per-room operation.
	///
	/// Acquire it before the state or timeline insertion mutex for the same
	/// room. The canonical order is federation, state, then insertion.
	pub mutex_federation: RoomMutexMap,
	services: Arc<crate::services::OnceServices>,
	db: Data,
	state_local: Arc<StateLocalCounters>,
	prev_walk: PrevWalkCounters,

	/// Gapped incoming events whose passes are in flight.
	///
	/// An entry lives exactly as long as its pass. Top-level timeline passes run
	/// under the room's `mutex_federation` except for remote invites and a local
	/// join's federation fallback, so entries stay within one per room holding its
	/// federation mutex, plus any passes those two callers have in flight.
	prev_walks_in_flight: InFlightWalks,

	backoff: BackoffCounters,
}

struct Data {
	eventid_backoff: Arc<Map>,
	eventid_policysigstate: Arc<Map>,
	eventid_resolvedstate: Arc<Map>,
	roomtseventid_prevwalk: Arc<Map>,
}

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;

// Distinct candidate servers tried per fetch, not retries per server.
const EVENT_FETCH_ATTEMPT_LIMIT: NonZeroUsize = NonZeroUsize::new(3).unwrap();

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			mutex_federation: RoomMutexMap::new(),
			services: args.services.clone(),
			state_local: Arc::new(StateLocalCounters::default()),
			prev_walk: PrevWalkCounters::default(),
			prev_walks_in_flight: InFlightWalks::default(),
			backoff: BackoffCounters::default(),
			db: Data {
				eventid_backoff: args.db["eventid_backoff"].clone(),
				eventid_policysigstate: args.db["eventid_policysigstate"].clone(),
				eventid_resolvedstate: args.db["eventid_resolvedstate"].clone(),
				roomtseventid_prevwalk: args.db["roomtseventid_prevwalk"].clone(),
			},
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex_federation = self.mutex_federation.len();

		writeln!(out, "- federation_mutex: {mutex_federation}")?;

		let prev_walks_in_flight = self.prev_walks_in_flight_count();

		writeln!(out, "- prev_walks_in_flight: {prev_walks_in_flight}")?;

		Ok(())
	}

	async fn clear_cache(&self) {
		self.db.eventid_backoff.clear().await;
		self.db.eventid_resolvedstate.clear().await;
	}

	fn name(&self) -> &str { make_name(module_path!()) }
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

/// Extract a room's version from the create event in a stripped-state list (as
/// stored for an out-of-band invite or knock).
fn room_version_of(stripped: &[Raw<AnyStrippedStateEvent>]) -> Option<RoomVersionId> {
	stripped
		.iter()
		.find_map(|event| match event.deserialize() {
			| Ok(AnyStrippedStateEvent::RoomCreate(create)) => Some(create.content.room_version),
			| _ => None,
		})
}
