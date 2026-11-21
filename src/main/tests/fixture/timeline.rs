//! Whether an event reached the room timeline.
//!
//! Only a binary declaring it by path compiles it, so the boot fixture's other
//! binaries stay free of its dead code.

use tuwunel_core::{Result, result::NotFound, ruma::EventId};
use tuwunel_service::Services;

/// Whether the event is in its room's timeline, as an accepted event is.
///
/// Only a missing record reads as absent. Any other failure, an I/O not-found
/// included, is an error, so a storage fault cannot pass for a refusal or an
/// acceptance.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn timelined(services: &Services, event_id: &EventId) -> Result<bool> {
	services
		.timeline
		.non_outlier_pdu_exists(event_id)
		.await
		.present()
		.map(|exists| exists.is_some())
}
