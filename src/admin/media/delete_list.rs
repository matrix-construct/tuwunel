use ruma::Mxc;
use tuwunel_core::{Err, Result, info, trace, utils::math::Expected, warn};

use crate::admin_command;

#[admin_command]
pub(super) async fn delete_list(&self) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.",);
	}

	let mut failed_parsed_mxcs: usize = 0;

	let mxc_list = self
		.body
		.to_vec()
		.drain(1..self.body.len().expected_sub(1))
		.filter_map(|mxc_s| {
			mxc_s
				.try_into()
				.inspect_err(|e| {
					warn!("Failed to parse user-provided MXC URI: {e}");
					failed_parsed_mxcs = failed_parsed_mxcs.saturating_add(1);
				})
				.ok()
		})
		.collect::<Vec<Mxc<'_>>>();

	let mut mxc_deletion_count: usize = 0;

	for mxc in &mxc_list {
		trace!(%failed_parsed_mxcs, %mxc_deletion_count, "Deleting MXC {mxc} in bulk");
		match self.services.media.delete(mxc).await {
			| Ok(()) => {
				info!("Successfully deleted {mxc} from filesystem and database");
				mxc_deletion_count = mxc_deletion_count.saturating_add(1);
			},
			| Err(e) => {
				warn!("Failed to delete {mxc}, ignoring error and skipping: {e}");
			},
		}
	}

	write!(
		self,
		"Finished bulk MXC deletion, deleted {mxc_deletion_count} total MXCs from our database \
		 and the filesystem. {failed_parsed_mxcs} MXCs failed to be parsed from the database.",
	)
	.await
}
