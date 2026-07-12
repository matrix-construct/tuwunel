use ruma::OwnedEventId;
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn state_at_incoming(&self, event_id: OwnedEventId) -> Result {
	let report = self
		.services
		.event_handler
		.local_state_report(&event_id)
		.await?;

	let out = format!("```\n{report:#?}\n```");

	self.write_str(&out).await
}
