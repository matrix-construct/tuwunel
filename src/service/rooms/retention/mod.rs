use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{Stream, TryStreamExt};
use ruma::{CanonicalJsonObject, EventId};
use tuwunel_core::{
	Result, debug_info, expected, implement,
	matrix::pdu::PduEvent,
	utils::{TryReadyExt, time::now},
};
use tuwunel_database::{Deserialized, Json, Map};

use crate::rooms::timeline::RoomMutexGuard;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	eventid_originalpdu: Arc<Map>,
	timeredacted_eventid: Arc<Map>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			eventid_originalpdu: args.db["eventid_originalpdu"].clone(),
			timeredacted_eventid: args.db["timeredacted_eventid"].clone(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		loop {
			let retention_seconds = self.services.config.redaction_retention_seconds;

			if retention_seconds != 0 {
				debug_info!("Cleaning up retained events");

				let now = now().as_secs();
				let count = self
					.timeredacted_eventid
					.keys::<(u64, &EventId)>()
					.ready_try_take_while(|(time_redacted, _)| {
						let time_redacted = *time_redacted;
						Ok(expected!(time_redacted + retention_seconds) < now)
					})
					.ready_try_fold_default(|count: usize, (time_redacted, event_id)| {
						self.eventid_originalpdu.remove(event_id);
						self.timeredacted_eventid
							.del((time_redacted, event_id));
						Ok(count.saturating_add(1))
					})
					.await?;

				debug_info!(?count, "Finished cleaning up retained events");
			}

			tokio::select! {
				() = tokio::time::sleep(Duration::from_hours(1)) => {},
				() = self.services.server.until_shutdown() => return Ok(())
			};
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
pub async fn get_original_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
	self.eventid_originalpdu
		.get(event_id)
		.await?
		.deserialized()
}

#[implement(Service)]
pub async fn get_original_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.eventid_originalpdu
		.get(event_id)
		.await?
		.deserialized()
}

#[implement(Service)]
pub async fn save_original_pdu(
	&self,
	event_id: &EventId,
	pdu: &CanonicalJsonObject,
	_state_lock: &RoomMutexGuard,
) {
	if !self.services.config.save_unredacted_events {
		return;
	}

	if self
		.eventid_originalpdu
		.exists(event_id)
		.await
		.is_ok()
	{
		return;
	}

	let now = now().as_secs();

	self.eventid_originalpdu
		.raw_put(event_id, Json(pdu));

	self.timeredacted_eventid
		.put_raw((now, event_id), []);
}

#[implement(Service)]
pub fn retained_pdus_raw(&self) -> impl Stream<Item = Result<&[u8]>> + Send {
	self.eventid_originalpdu
		.raw_stream()
		.map_ok(|x| x.1)
}
