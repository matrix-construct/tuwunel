use tuwunel_core::{Result, utils::time::parse_timepoint_ago};

use crate::admin_command;

#[admin_command]
pub(super) async fn delete_range(
	&self,
	duration: String,
	older_than: bool,
	newer_than: bool,
	yes_i_want_to_delete_local_media: bool,
) -> Result {
	let duration = parse_timepoint_ago(&duration)?;
	let deleted_count = self
		.services
		.media
		.delete_range(duration, older_than, newer_than, yes_i_want_to_delete_local_media)
		.await?;

	write!(self, "Deleted {deleted_count} total files.").await
}
