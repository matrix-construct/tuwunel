use tuwunel_core::{Result, debug, info, utils::TryReadyExt, warn};
use tuwunel_database::Map;

use crate::{
	Services,
	integrity::referencedevents::{Finding, scan},
};

pub(super) async fn fix_referencedevents_missing_sep(services: &Services) -> Result {
	warn!("Fixing missing record separator between room_id and event_id in referencedevents");

	let db = &services.db;
	let referencedevents = &db["referencedevents"];
	let cork = db.cork_and_sync();

	let (fixed, unparsable) = scan(services)
		.ready_try_fold((0_usize, 0_usize), |totals, finding| {
			Ok(repair(referencedevents, totals, &finding))
		})
		.await?;

	drop(cork);
	info!(%fixed, %unparsable, "Fixed missing record separators in 'referencedevents'.");

	referencedevents.sort()
}

fn repair(
	referencedevents: &Map,
	(fixed, unparsable): (usize, usize),
	finding: &Finding,
) -> (usize, usize) {
	match finding {
		| Finding::Unparsable { .. } => (fixed, unparsable.saturating_add(1)),
		| Finding::Unseparated { key, room_id, event_id } => {
			debug!(%room_id, %event_id, "fixing");

			referencedevents.put_raw((room_id.as_str(), event_id.as_str()), []);
			referencedevents.remove(key);

			(fixed.saturating_add(1), unparsable)
		},
	}
}
