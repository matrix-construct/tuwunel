use std::sync::Arc;

use ruma::{
	events::{AnySyncMessageLikeEvent, room::encrypted::Relation},
	serde::Raw,
};
use serde::Deserialize;
use tuwunel_core::{Result, matrix::Pdu};
use tuwunel_database::Map;

mod bundling;
mod purge;
mod references;
mod relations;
mod typed_relations;

#[cfg(test)]
mod tests;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	tofrom_relation: Arc<Map>,
	relatesto_typed: Arc<Map>,
	referencedevents: Arc<Map>,
	softfailedeventids: Arc<Map>,
}

#[derive(Deserialize)]
struct ExtractRelatesTo {
	#[serde(rename = "m.relates_to")]
	relates_to: Relation,
}

/// Requester-specific MSC3856 adjustment for one thread root in `/threads`.
///
/// Root redaction, visible replies, latest event, and reply count depend on the
/// requesting user and their ignore list. The resulting view must not be reused
/// across requesters.
///
/// `Unchanged` preserves the served root, while `Omitted` removes it from the
/// response. Each `Adjusted` field is `None` when that facet needs no change. A
/// supplied `root` provides redacted content while the caller retains the
/// served `unsigned`.
pub enum IgnoredThreadView {
	Unchanged,
	Omitted,
	Adjusted {
		root: Option<Box<Pdu>>,
		count: Option<usize>,
		latest: Option<Raw<AnySyncMessageLikeEvent>>,
	},
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data {
				tofrom_relation: args.db["tofrom_relation"].clone(),
				relatesto_typed: args.db["relatesto_typed"].clone(),
				referencedevents: args.db["referencedevents"].clone(),
				softfailedeventids: args.db["softfailedeventids"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}
