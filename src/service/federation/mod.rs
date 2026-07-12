mod execute;
mod format;
mod peer;
mod rank;
pub mod scheme;
#[cfg(test)]
mod tests;

use std::{sync::Arc, time::Duration};

use tuwunel_core::{Result, utils::exponential_backoff_streak_cap};
use tuwunel_database::Map;

use self::peer::MAX_BACKOFF;
pub use self::{
	peer::{Classification, ShouldAttempt},
	rank::{Candidates, WhenAllBackedOff},
};
use crate::services::OnceServices;

pub struct Service {
	services: Arc<OnceServices>,
	statuses: Arc<Map>,

	/// Width of one peer-status bucket in seconds, aligned with
	/// `sender_timeout` so the streak (the window span between a peer's oldest
	/// and newest recorded failure) tracks the sender's `consecutive_failures`
	/// notion at the cutover.
	window_secs: u64,

	/// Streak cap = `ceil(sqrt(MAX_BACKOFF / window_secs))`. Past this span the
	/// quadratic curve `window * n²` saturates at [`MAX_BACKOFF`], so a longer
	/// streak cannot change the verdict.
	n_max: u32,

	/// Grace before the first retry of a once-failed peer, snapshot from
	/// `sender_retry_grace`. Zero disables the grace tier so the plain bucket
	/// curve governs from the first failure.
	grace: Duration,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let window_secs = args.server.config.sender_timeout.max(1);
		let n_max = exponential_backoff_streak_cap(Duration::from_secs(window_secs), MAX_BACKOFF);
		let grace = Duration::from_secs(args.server.config.sender_retry_grace);

		Ok(Arc::new(Self {
			services: args.services.clone(),
			statuses: args.db["servername_status"].clone(),
			window_secs,
			n_max,
			grace,
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}
