//! Checks timeline reads and key-boundary behavior.
//!
//! Missing and unreadable rows remain distinct across accepted and outlier
//! storage. Forward scans preserve the lowest valid backfilled count.

use ruma::event_id;
use tuwunel_core::{Error, Result, config::Figment, utils::result::NotFound};

use super::{Direction, PduCount, Service};
use crate::test_utils::{fixture, pdu_id};

#[tokio::test]
async fn get_preserves_unreadable_rows() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let timeline = &services.timeline;
	let event = event_id!("$lookup:localhost");
	let id = pdu_id(1);
	let valid: &[u8] = b"{}";
	let corrupt: &[u8] = b"{";
	let cases = [
		(Some(corrupt), None, "corrupt"),
		(None, Some(corrupt), "corrupt"),
		(Some(corrupt), Some(corrupt), "corrupt"),
		(None, None, "missing"),
		(Some(valid), None, "found"),
		(None, Some(valid), "found"),
		(Some(valid), Some(corrupt), "found"),
		(Some(corrupt), Some(valid), "found"),
		(Some(valid), Some(valid), "found"),
	];

	for (accepted, outlier, expected) in cases {
		services.db["eventid_pduid"].remove(event);
		services.db["pduid_pdu"].remove(&id);
		services.db["eventid_outlierpdu"].remove(event);
		if let Some(bytes) = accepted {
			services.db["eventid_pduid"].insert(event, id.as_bytes());
			services.db["pduid_pdu"].insert(&id, bytes);
		}

		if let Some(bytes) = outlier {
			services.db["eventid_outlierpdu"].insert(event, bytes);
		}

		// Warm both lookups so the accepted failure precedes the missing outlier.
		timeline
			.get_non_outlier_pdu_json(event)
			.await
			.ok();

		timeline.get_outlier_pdu_json(event).await.ok();

		let result = timeline.get_pdu_json(event).await.present();
		let outcome = match result {
			| Ok(Some(_)) => "found",
			| Ok(None) => "missing",
			| Err(Error::Json(_)) => "corrupt",
			| Err(error) => return Err(error),
		};

		assert_eq!(outcome, expected, "accepted={accepted:?}, outlier={outlier:?}");
	}

	Ok(())
}

#[test]
fn forward_backfilled_zero_stays_at_lower_bound() {
	let pdu_id = Service::pdu_count_to_id(1, PduCount::Backfilled(0), Direction::Forward);

	assert_eq!(pdu_id.pdu_count(), PduCount::Backfilled(0));
}
