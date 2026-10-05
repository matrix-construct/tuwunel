use futures::TryStreamExt;
use ruma::{EventId, event_id};
use tuwunel_core::{Result, config::Figment, smallvec::SmallVec};
use tuwunel_database::{Ignore, Interfix};

use super::{BackoffCounters, BackoffMetrics, Context, Disposition, Suppression, Verdicts};
use crate::{rooms::event_handler::Service, test_utils::fixture};

type Records = SmallVec<[(u64, u64); 1]>;

#[test]
fn each_verdict_counts_in_its_own_cell() {
	let counters = BackoffCounters::default();
	let lookups = [
		(Context::Fetch, Suppression::Absent),
		(Context::Auth, Suppression::Allow),
		(Context::Upgrade, Suppression::Deny),
		(Context::Upgrade, Suppression::Absent),
		(Context::Incoming, Suppression::Deny),
		(Context::Incoming, Suppression::Deny),
	];

	for (ctx, verdict) in &lookups {
		counters.count(*ctx, verdict);
	}

	let expected = BackoffMetrics {
		fetch: Verdicts { absent: 1, allowed: 0, denied: 0 },
		auth: Verdicts { absent: 0, allowed: 1, denied: 0 },
		upgrade: Verdicts { absent: 1, allowed: 0, denied: 1 },
		incoming: Verdicts { absent: 0, allowed: 0, denied: 2 },
	};

	assert_eq!(counters.snapshot(), expected);
}

#[tokio::test]
async fn shutdown_preserves_pending_instead_of_recording_failure() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let handler = &fixture.services.event_handler;
	let contexts = [Context::Auth, Context::Fetch, Context::Upgrade];
	let normal = event_id!("$normal");
	let pending = event_id!("$pending");
	let absent = event_id!("$absent");
	let cancelled = event_id!("$cancelled");

	for ctx in contexts {
		handler.record_outcome(ctx, normal, Disposition::Transient);

		let recorded = records(handler, ctx, normal).await?;

		assert_eq!(recorded.len(), 1);
		assert_eq!(recorded[0].0, u64::from(Disposition::Transient));
		handler.record_attempt(ctx, pending);
	}

	fixture.services.server.shutdown()?;
	assert!(fixture.services.server.is_stopping());

	for ctx in contexts {
		let before = records(handler, ctx, pending).await?;

		assert_eq!(before.len(), 1);
		assert_eq!(before[0].0, u64::from(Disposition::Pending));
		handler.record_outcome(ctx, pending, Disposition::Transient);
		handler.record_outcome(ctx, absent, Disposition::Transient);
		assert_eq!(records(handler, ctx, pending).await?, before);
		assert_eq!(records(handler, ctx, absent).await?.len(), 0);
		handler.record_attempt(ctx, cancelled);

		let attempted = records(handler, ctx, cancelled).await?;

		assert_eq!(attempted.len(), 1);
		assert_eq!(attempted[0].0, u64::from(Disposition::Pending));
	}

	Ok(())
}

#[tracing::instrument(level = "trace", skip(handler, ctx))]
async fn records(handler: &Service, ctx: Context, event_id: &EventId) -> Result<Records> {
	handler
		.db
		.eventid_backoff
		.stream_prefix(&(u8::from(ctx), event_id, Interfix))
		.map_ok(|(_, value): (Ignore, (u64, u64))| value)
		.try_collect()
		.await
}
