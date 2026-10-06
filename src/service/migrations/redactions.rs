use futures::{TryFutureExt, future::ready};
use ruma::events::TimelineEventType;
use serde::Deserialize;
use tuwunel_core::{
	Err, Result, err, info,
	matrix::{event::Event, pdu::PduEvent, room_version::rules as room_rules},
	utils::{TryReadyExt, stream::TryBroadbandExt},
	warn,
};

use super::{marker_present, rebuild_thread_summaries, scan::ScanExt};
use crate::Services;

/// Stamped once the timeline has been checked for redactions applied without
/// being recorded.
pub(super) const MARKER: &str = "restore_foreign_redactions";

/// Stamped by the origin when its rewrite of the stored redactions completes.
const ORIGIN_MARKER: &str = "unembed_unsigned_info";

/// Created on every open by an origin release that records redactions by
/// reference.
const ORIGIN_COLUMN: &str = "servernamekeyid_response";

/// The event type of a stored row, read without the rest of the event.
///
/// The walk reads every timeline row, so only a row this shows to be a
/// redaction pays for the full parse.
#[derive(Deserialize)]
struct Kind {
	#[serde(rename = "type")]
	kind: TimelineEventType,
}

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
	if !origin_records_by_reference(services).await? {
		return Ok(());
	}

	let cork = services.db.cork_and_sync();

	let Tally { restored, skipped, failed, unreadable } = services.db["pduid_pdu"]
		.raw_stream()
		.scanned(&services.server)
		.ready_try_filter_map(|(_, value)| Ok(redaction(value)))
		.broad_and_then(async move |redaction| {
			let outcome = ready(redaction)
				.and_then(|redaction| restore(services, redaction))
				.await;

			Ok(outcome)
		})
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

/// Whether the origin ever opened the database with a release that records
/// redactions by reference.
///
/// The stamp lands only when the origin finishes rewriting the stored rows,
/// while its redactions take the new form from the first open, so the column
/// that release creates on every open answers for a rewrite never finished.
async fn origin_records_by_reference(services: &Services) -> Result<bool> {
	if services.db.open_cf(ORIGIN_COLUMN)?.is_some() {
		return Ok(true);
	}

	marker_present(services, ORIGIN_MARKER).await
}

/// The redaction event a timeline row holds.
///
/// Any other row is `None`, as is one whose type cannot be read, since nothing
/// can be restored from it; a redaction that cannot be parsed is an error the
/// walk counts.
fn redaction(value: &[u8]) -> Option<Result<PduEvent>> {
	serde_json::from_slice(value)
		.is_ok_and(|probe: Kind| probe.kind == TimelineEventType::RoomRedaction)
		.then(|| {
			serde_json::from_slice(value)
				.map_err(|e| err!(Database("unparsable redaction event: {e}")))
				.inspect_err(|error| warn!(%error, "A stored redaction event could not be read"))
		})
}

/// Records one redaction on its target, reporting whether the target was
/// rewritten.
///
/// A failure is logged with the redaction's event id and returned for the walk
/// to count.
async fn restore(services: &Services, redaction: PduEvent) -> Result<bool> {
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
