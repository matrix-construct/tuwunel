use std::path::PathBuf;

use tuwunel_core::{Result, utils::time::now};

use crate::admin_command;

#[admin_command]
pub(super) async fn checkpoint_database(
	&self,
	map: Option<String>,
	path: Option<PathBuf>,
	log_size: u64,
) -> Result {
	let path = path.unwrap_or_else(|| {
		let epoch = now().as_secs();

		self.services
			.server
			.config
			.database_path
			.join(format!("checkpoint-{epoch}"))
	});

	let path = self
		.blocking_db(move |db| {
			match map {
				| None => db.engine.checkpoint(&path, log_size)?,
				| Some(map) => db.get(&map)?.checkpoint(&path)?,
			}

			Ok(path)
		})
		.await?;

	write!(self, "Created checkpoint at `{}`.", path.display()).await
}
