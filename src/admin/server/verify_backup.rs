use std::sync::Arc;

use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn verify_backup(&self, backup_id: Option<u32>) -> Result {
	let db = Arc::clone(&self.services.db);
	let backup_id = backup_id.unwrap_or(0);
	let result = self
		.services
		.server
		.runtime()
		.spawn_blocking(move || match db.engine.backup_verify(backup_id) {
			| Ok(id) => format!("Verified backup #{id}: all files present with expected sizes."),
			| Err(e) => format!("Failed: {e}"),
		})
		.await?;

	write!(self, "{result}").await
}
