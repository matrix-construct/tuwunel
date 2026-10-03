use ruma::{EventId, OwnedEventId, OwnedRoomOrAliasId};
use tuwunel_core::{
	Result, debug_warn,
	utils::{ReadyExt, stream::BroadbandExt},
};
use tuwunel_matrix::Event;
use tuwunel_service::Services;

use crate::admin_command;

#[derive(Default)]
struct ClearSummary {
	cleared: usize,
	unreadable: usize,
}

#[admin_command]
pub(super) async fn room_clear_soft_failed_events(&self, room_id: OwnedRoomOrAliasId) -> Result {
	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	let _federation_lock = self
		.services
		.event_handler
		.mutex_federation
		.lock(&room_id)
		.await;

	let clear =
		async |event_id: OwnedEventId| match self.services.timeline.get_pdu(&event_id).await {
			| Ok(pdu) if pdu.room_id() == room_id => clear_event(self.services, &event_id).await,
			| Ok(_) => ClearSummary::default(),
			| Err(error) => {
				debug_warn!(%event_id, %error, "Unable to read soft-failed event");
				ClearSummary { unreadable: 1, ..Default::default() }
			},
		};

	let ClearSummary { cleared, unreadable } = self
		.services
		.pdu_metadata
		.soft_failed_event_ids()
		.broad_then(clear)
		.ready_fold(ClearSummary::default(), ClearSummary::merge)
		.await;

	write!(
		self,
		"Cleared {cleared} soft-failed event markers for {room_id} and removed any \
		 corresponding cached policy decisions. Events will be checked again when federation \
		 supplies them. Skipped {unreadable} markers whose events could not be read."
	)
	.await
}

/// Drops every stored verdict gating one event's re-evaluation.
///
/// The cached policy decision, the retry backoff, and the soft-fail marker each
/// hold the next delivery back, so recovery has to clear all three. Dropping
/// the backoff is what opens the gate, and it follows the policy decision, so
/// an interrupted run never re-evaluates against a stale verdict.
async fn clear_event(services: &Services, event_id: &EventId) -> ClearSummary {
	services
		.event_handler
		.clear_policy_signature_state(event_id);

	services
		.event_handler
		.clear_delivery_backoff(event_id)
		.await;

	services
		.pdu_metadata
		.clear_event_soft_failed(event_id);

	ClearSummary { cleared: 1, ..Default::default() }
}

impl ClearSummary {
	fn merge(mut self, Self { cleared, unreadable }: Self) -> Self {
		self.cleared = self.cleared.saturating_add(cleared);
		self.unreadable = self.unreadable.saturating_add(unreadable);
		self
	}
}
