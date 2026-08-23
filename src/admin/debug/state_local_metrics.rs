use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
/// Print process-lifetime local state-build counters.
///
/// Difference two snapshots to observe activity over an interval.
pub(super) async fn state_local_metrics(&self) -> Result {
	let metrics = self.services.event_handler.state_local_metrics();
	let out = format!(
		"State-local build counters are process-lifetime totals. Two snapshots should be \
		 differenced to obtain an interval.\n\n```rs\n{metrics:#?}\n```"
	);

	self.write_str(&out).await
}
