use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn backup_database(&self) -> Result {
	let count = self
		.blocking_db(|db| {
			db.engine.backup()?;
			db.engine.backup_count()
		})
		.await?;

	write!(self, "Done. Currently have {count} backups.").await
}
