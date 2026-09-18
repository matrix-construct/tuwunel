//! Checks timeline key-boundary behavior.
//!
//! The focused regression test ensures a forward scan preserves the lowest
//! valid backfilled count when deriving its database start key.

use super::{Direction, PduCount, Service};

#[test]
fn forward_backfilled_zero_stays_at_lower_bound() {
	let pdu_id = Service::pdu_count_to_id(1, PduCount::Backfilled(0), Direction::Forward);

	assert_eq!(pdu_id.pdu_count(), PduCount::Backfilled(0));
}
