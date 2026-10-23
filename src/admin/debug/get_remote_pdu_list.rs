use ruma::{EventId, OwnedServerName};
use tuwunel_core::{Err, Result, utils::string::EMPTY, warn};

use crate::admin_command;

#[admin_command]
pub(super) async fn get_remote_pdu_list(&self, server: OwnedServerName, force: bool) -> Result {
	self.services.server.config.require_federation()?;

	if server == self.services.globals.server_name() {
		return Err!(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for \
			 fetching local PDUs from the database.",
		);
	}

	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&EMPTY).trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.",);
	}

	let list = self
		.body
		.iter()
		.collect::<Vec<_>>()
		.drain(1..self.body.len().saturating_sub(1))
		.filter_map(|pdu| EventId::parse(pdu).ok())
		.collect::<Vec<_>>();

	let mut failed_count: usize = 0;
	let mut success_count: usize = 0;

	for event_id in list {
		let result = self
			.get_remote_pdu(event_id, server.clone())
			.await;

		if !force {
			result?;
		} else if let Err(e) = result {
			warn!("Failed to get remote PDU, ignoring error: {e}");
			failed_count = failed_count.saturating_add(1);
			continue;
		}

		success_count = success_count.saturating_add(1);
	}

	let out =
		format!("Fetched {success_count} remote PDUs successfully with {failed_count} failures");

	self.write_str(&out).await
}
