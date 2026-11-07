use tuwunel_core::{Result, debug, info, utils::TryReadyExt, warn};
use tuwunel_database::Map;

use crate::{
	Services,
	integrity::readreceiptid_readreceipt::{Finding, scan},
};

pub(super) async fn fix_readreceiptid_readreceipt_duplicates(services: &Services) -> Result {
	warn!("Fixing undeleted entries in readreceiptid_readreceipt...");

	let db = &services.db;
	let readreceiptid_readreceipt = &db["readreceiptid_readreceipt"];
	let cork = db.cork_and_sync();

	let (fixed, unparsable) = scan(services)
		.ready_try_fold((0_usize, 0_usize), |totals, finding| {
			Ok(repair(readreceiptid_readreceipt, totals, &finding))
		})
		.await?;

	drop(cork);
	info!(%fixed, %unparsable, "Fixed undeleted entries in readreceiptid_readreceipt.");

	readreceiptid_readreceipt.sort()
}

fn repair(
	readreceiptid_readreceipt: &Map,
	(fixed, unparsable): (usize, usize),
	finding: &Finding,
) -> (usize, usize) {
	match finding {
		| Finding::Unparsable { .. } => (fixed, unparsable.saturating_add(1)),
		| Finding::Repeated { key, room_id, user_id } => {
			debug!(%room_id, %user_id, "removing");

			readreceiptid_readreceipt.remove(key);

			(fixed.saturating_add(1), unparsable)
		},
	}
}
