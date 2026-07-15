use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn verify_backup(&self, backup_id: Option<u32>) -> Result {
	let backup_id = backup_id.unwrap_or(0);
	let id = self
		.blocking_db(move |db| db.engine.backup_verify(backup_id))
		.await?;

	write!(self, "Verified backup #{id}: all files present with expected sizes.").await
}
