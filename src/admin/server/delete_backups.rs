use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn delete_backups(&self, keep: usize) -> Result {
	let count = self
		.blocking_db(move |db| {
			db.engine.backup_purge(keep)?;
			db.engine.backup_count()
		})
		.await?;

	write!(self, "Done. Currently have {count} backups.").await
}
