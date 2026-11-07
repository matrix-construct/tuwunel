#![cfg(test)]

use tuwunel_core::{Result, result::NotFound};
use tuwunel_service::Services;

use self::{admin::refused, fixture::boot, reply::replied};

#[expect(dead_code)] // Only refusals go through the shared admin assertions.
mod admin;
#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;
mod fixture;
#[path = "admin/reply.rs"]
mod reply;

const CONFIRM: &str = "--yes-i-know-what-im-doing";
const MARKER: &str = "rebuild_relatesto_typed";

/// `integrity repair` clears a step's stamp only when confirmed and safe.
///
/// Without the confirmation flag, or for a check whose step must not run twice,
/// the stamp stays. A cleared stamp reads as pending to the `schema` check, and
/// clearing it again reports the step as already pending.
#[test]
fn integrity_repair_clears_only_confirmed_stamps() -> Result {
	boot("integrity-repair", [], exercise)
}

async fn exercise(services: &Services, _: &str) -> Result {
	refused(services, "integrity repair relation-index", CONFIRM).await?;
	assert!(stamped(services).await?, "an unconfirmed repair must keep {MARKER}");

	let short_ids = format!("integrity repair short-ids {CONFIRM}");

	refused(services, &short_ids, "the outcome it records").await?;

	let repair = format!("integrity repair relation-index {CONFIRM}");
	let cleared = replied(services, &repair).await?;

	assert!(cleared.contains(&format!("- {MARKER}: cleared")), "{cleared}");
	assert!(!stamped(services).await?, "a confirmed repair must clear {MARKER}");

	let pending = replied(services, &repair).await?;

	assert!(pending.contains(&format!("- {MARKER}: already pending")), "{pending}");

	let schema = replied(services, "integrity check schema").await?;

	assert!(schema.contains(&format!("migration {MARKER} is not stamped")), "{schema}");

	Ok(())
}

async fn stamped(services: &Services) -> Result<bool> {
	services.db["global"]
		.get(MARKER)
		.await
		.optional()
		.map(|stamp| stamp.is_some())
}
