use ruma::OwnedServerName;
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn peer_status_record_success(&self, server_name: OwnedServerName) -> Result {
	self.services
		.federation
		.record_success(&server_name)
		.await;

	write!(self, "Cleared status for {server_name}.").await
}
