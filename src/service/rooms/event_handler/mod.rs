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
mod resolve_state;
mod state_at_incoming;
mod state_local_build;
mod upgrade_outlier_pdu;

use std::{fmt::Write, num::NonZeroUsize, sync::Arc};

use async_trait::async_trait;
use ruma::{EventId, OwnedRoomId, RoomVersionId, events::AnyStrippedStateEvent, serde::Raw};
use tuwunel_core::{Result, implement, matrix::PduEvent, utils::MutexMap};
use tuwunel_database::Map;

pub use self::{policy_server::PolicyCheck, state_local_build::LocalBuildReport};

pub struct Service {
	pub mutex_federation: RoomMutexMap,
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	eventid_backoff: Arc<Map>,
	eventid_policysigstate: Arc<Map>,
	eventid_resolvedstate: Arc<Map>,
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
			db: Data {
				eventid_backoff: args.db["eventid_backoff"].clone(),
				eventid_policysigstate: args.db["eventid_policysigstate"].clone(),
				eventid_resolvedstate: args.db["eventid_resolvedstate"].clone(),
			},
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex_federation = self.mutex_federation.len();
		writeln!(out, "- federation_mutex: {mutex_federation}")?;

		Ok(())
	}

	async fn clear_cache(&self) {
		self.db.eventid_backoff.clear().await;
		self.db.eventid_resolvedstate.clear().await;
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
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
