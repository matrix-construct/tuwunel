//! Synapse admin API: federation endpoints.

mod destination;
mod destination_rooms;
mod list_destinations;
mod reset_connection;

use ruma::{OwnedServerName, UInt};
use synapse_admin_api::federation::Destination;
use tuwunel_service::federation::PeerBackoff;

pub(crate) use self::{
	destination::admin_destination_details_route,
	destination_rooms::admin_destination_rooms_route,
	list_destinations::admin_list_destinations_route,
	reset_connection::admin_reset_connection_route,
};

/// A healthy peer (no failure rows) keeps the all-zero destination defaults; a
/// failing one carries its mapped retry timings.
pub(crate) fn destination_from_backoff(
	destination: OwnedServerName,
	backoff: Option<PeerBackoff>,
) -> Destination {
	let Some(backoff) = backoff else {
		return Destination::new(destination);
	};

	Destination {
		retry_last_ts: millis(backoff.anchor_secs),
		retry_interval: millis(backoff.delay_secs),
		failure_ts: Some(millis(backoff.oldest_secs)),
		..Destination::new(destination)
	}
}

fn millis(secs: u64) -> UInt { UInt::new_saturating(secs.saturating_mul(1000)) }
