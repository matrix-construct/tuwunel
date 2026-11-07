use tuwunel_core::{Result, info, utils::TryReadyExt, warn};
use tuwunel_database::Map;

use super::tally::{Outcome, Tally};
use crate::{
	Services,
	integrity::roomid_tscount_pducount::{Expected, expected},
};

/// Writes the timestamp index row of every timeline event.
///
/// The rows are the ones the timestamp index check expects, written over the
/// index without clearing it. An event whose key is not a PDU id is skipped.
pub(super) async fn rebuild_roomid_tscount_pducount(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let index = &db["roomid_tscount_pducount"];

	warn!("Rebuilding roomid_tscount_pducount index for same-timestamp event ordering");

	let tally = expected(services)
		.ready_try_fold(Tally::default(), |tally, expected| {
			Ok(tally.record(write(index, &expected)))
		})
		.await?;

	drop(cork);
	info!(
		count = %tally.repaired,
		skipped = %tally.skipped,
		"Rebuilt roomid_tscount_pducount index"
	);

	index.sort()
}

fn write(index: &Map, expected: &Expected) -> Outcome {
	match expected {
		| Expected::Unparsable(_) => Outcome::Skipped,
		| Expected::Row(row) => {
			index.put_raw(row.key(), row.pdu_id.count());

			Outcome::Repaired
		},
	}
}
