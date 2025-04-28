use std::sync::Arc;

use ruma::{CanonicalJsonObject, EventId};
use tuwunel_core::{Result, implement, matrix::pdu::PduEvent};
use tuwunel_database::{Deserialized, Json, Map};

pub struct Service {
	db: Data,
}

struct Data {
	eventid_outlierpdu: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				eventid_outlierpdu: args.db["eventid_outlierpdu"].clone(),
			},
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

/// Returns the pdu from the outlier tree.
#[implement(Service)]
pub async fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.db
		.eventid_outlierpdu
		.get(event_id)
		.await
		.deserialized()
}

/// Returns the pdu from the outlier tree.
#[implement(Service)]
pub async fn get_pdu_outlier(&self, event_id: &EventId) -> Result<PduEvent> {
	self.db
		.eventid_outlierpdu
		.get(event_id)
		.await
		.deserialized()
}

/// Append the PDU as an outlier.
#[implement(Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub fn add_pdu_outlier(&self, event_id: &EventId, pdu: &CanonicalJsonObject) {
	self.db
		.eventid_outlierpdu
		.raw_put(event_id, Json(pdu));
}
