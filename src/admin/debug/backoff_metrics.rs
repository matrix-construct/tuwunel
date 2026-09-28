use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
/// Print process-lifetime backoff verdict counters.
///
/// Difference two snapshots to observe activity over an interval.
pub(super) async fn backoff_metrics(&self) -> Result {
	let metrics = self.services.event_handler.backoff_metrics();
	let out = format!(
		"Backoff verdict counters are process-lifetime totals, one verdict per lookup in the \
		 backoff store before a federation step. Two snapshots should be differenced to obtain \
		 an interval.\n\n```rs\n{metrics:#?}\n```"
	);

	self.write_str(&out).await
}
