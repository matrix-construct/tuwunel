use tuwunel_core::{Result, info, utils::TryReadyExt};
use tuwunel_database::Map;

use super::tally::{Outcome, Tally};
use crate::{
	Services,
	integrity::userroomid_highlightcount::{Finding, scan},
};

/// Splits a Conduit database's conflated highlight-count column.
///
/// Conduit opens `roomuserid_lastnotificationread` against the
/// `userroomid_highlightcount` tree, a copy-paste in its schema, so one column
/// holds both stores. tuwunel keeps them apart under the same byte layouts, so
/// each room-keyed row moves verbatim into its own column and the user-keyed
/// highlight counts stay. A native database has no room-keyed row to move, so
/// it pays one row of the walk and takes no cork.
pub(super) async fn split_conduit_highlight_counts(services: &Services) -> Result {
	let db = &services.db;
	let highlight = &db["userroomid_highlightcount"];
	let lastread = &db["roomuserid_lastnotificationread"];

	// The cork is taken at the first finding and held to the end of the walk.
	let (tally, cork) = scan(services)
		.ready_try_fold((Tally::default(), None), |(tally, cork), finding| {
			let cork = cork.or_else(|| Some(db.cork_and_sync()));

			Ok((tally.record(repair(highlight, lastread, &finding)), cork))
		})
		.await?;

	drop(cork);

	if tally.repaired > 0 {
		info!(
			moved = %tally.repaired,
			"Split Conduit last-notification-read rows out of the highlight-count column"
		);
	}

	Ok(())
}

/// Moves the row a finding names into the last-read column.
///
/// The key alone decides, and the value moved is the one the walk read. The
/// insert comes first, so a stop between the two writes leaves the row to move
/// again on the next boot.
fn repair(highlight: &Map, lastread: &Map, Finding { key, value }: &Finding) -> Outcome {
	lastread.insert(key, value);
	highlight.remove(key);

	Outcome::Repaired
}
