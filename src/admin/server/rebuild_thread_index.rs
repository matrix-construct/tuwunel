use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn rebuild_thread_index(&self) -> Result {
	self.services
		.threads
		.rebuild_thread_activity()
		.await?;

	self.write_str("Rebuilt the thread activity index.")
		.await
}
