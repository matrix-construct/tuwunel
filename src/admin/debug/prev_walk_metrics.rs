use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
/// Print process-lifetime incoming prev-walk counters.
///
/// Difference two snapshots to observe activity over an interval.
pub(super) async fn prev_walk_metrics(&self) -> Result {
	let metrics = self.services.event_handler.prev_walk_metrics();
	let out = format!(
		"Prev-walk counters are process-lifetime totals. Two snapshots should be differenced to \
		 obtain an interval.\n\n```rs\n{metrics:#?}\n```"
	);

	self.write_str(&out).await
}
