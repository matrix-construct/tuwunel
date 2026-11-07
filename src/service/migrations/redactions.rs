use futures::TryFutureExt;
use tuwunel_core::{
	Err, Result, err, info,
	utils::{TryReadyExt, stream::TryBroadbandExt},
	warn,
};
use tuwunel_matrix::{event::Event, pdu::PduEvent, room_version::rules as room_rules};

use super::rebuild_thread_summaries;
use crate::{
	Services,
	integrity::redactions::{Redaction, Unparsable, records_by_reference, redactions},
};

/// Stamped once the timeline has been checked for redactions applied without
/// being recorded.
pub(super) const MARKER: &str = "restore_foreign_redactions";

/// Counts of what one walk of the timeline did.
///
/// A redaction the storage engine could not read transiently is counted apart
/// from one that failed for good, because only the first withholds the marker:
/// the walk is idempotent, so the next start retries it whole, while a corrupt
/// row would refuse every start.
#[derive(Default)]
struct Tally {
	restored: usize,
	skipped: usize,
	failed: usize,
	unreadable: usize,
}

/// Records each applied redaction that a foreign database left unrecorded on
/// the event it redacted.
///
/// Some origin releases keep only a reference to the redaction on the redacted
/// event, both when redacting and in a rewrite of every stored row, so on this
/// server the event reads as unredacted while its content is already gone.
/// Every stored redaction event is matched to its target, which is rewritten in
/// the form a redaction on this server leaves; thread summaries are then
/// rebuilt, also after a start that restored everything before being stopped,
/// so a redacted reply leaves its root's summary.
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn restore_redactions(services: &Services) -> Result {
	if !records_by_reference(services).await? {
		return Ok(());
	}

	let cork = services.db.cork_and_sync();

	// Redactions of one target can race, but each restore takes the room's lock
	// and decides again under it.
	let Tally { restored, skipped, failed, unreadable } = redactions(services)
		.broad_and_then(async |redaction| Ok(restore(services, redaction).await))
		.ready_try_fold(Tally::default(), |tally, outcome| Ok(tally.record(&outcome)))
		.await?;

	drop(cork);

	info!(
		%restored,
		%skipped,
		%failed,
		%unreadable,
		"Restored redactions a foreign database left unrecorded"
	);

	if unreadable > 0 {
		return Err!(Database("{unreadable} redactions could not be read"));
	}

	rebuild_thread_summaries(services).await;

	Ok(())
}

impl Tally {
	fn record(mut self, outcome: &Result<bool>) -> Self {
		match outcome {
			| Ok(true) => self.restored = self.restored.saturating_add(1),
			| Ok(false) => self.skipped = self.skipped.saturating_add(1),
			| Err(error) if error.is_transient_io() =>
				self.unreadable = self.unreadable.saturating_add(1),
			| Err(_) => self.failed = self.failed.saturating_add(1),
		}

		self
	}
}

/// Records one redaction on its target, reporting whether the target was
/// rewritten.
///
/// A failure, including a redaction that does not parse, is logged and returned
/// for the walk to count.
async fn restore(services: &Services, redaction: Redaction) -> Result<bool> {
	let redaction = match redaction {
		| Ok(redaction) => redaction,
		| Err(Unparsable { error, .. }) => {
			let error = err!(Database("unparsable redaction event: {error}"));

			warn!(%error, "A stored redaction event could not be read");
			return Err(error);
		},
	};

	restore_target(services, &redaction)
		.inspect_err(|error| {
			warn!(event_id = %redaction.event_id(), %error, "A redaction could not be restored");
		})
		.await
}

async fn restore_target(services: &Services, redaction: &PduEvent) -> Result<bool> {
	let room_id = redaction.room_id();
	let room_version = services.state.get_room_version(room_id).await?;
	let rules = room_rules(&room_version)?;
	let Some(target) = redaction.redacts_id(&rules) else {
		return Ok(false);
	};

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.restore_redaction(&target, redaction, &rules.redaction, &state_lock)
		.await
}
