use ruma::{OwnedDeviceId, OwnedUserId};
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn drop_connections(
	&self,
	user_id: Option<OwnedUserId>,
	device_id: Option<OwnedDeviceId>,
	conn_id: Option<String>,
) -> Result {
	let dropped = self
		.services
		.sync
		.clear_connections(
			user_id.as_deref(),
			device_id.as_deref(),
			conn_id.map(Into::into).as_ref(),
		)
		.await;

	write!(self, "Dropped {dropped} connections.").await
}
