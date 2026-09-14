use ruma::{OwnedDeviceId, OwnedUserId};
use tuwunel_core::{Err, Result};

use crate::admin_command;

#[admin_command]
pub(super) async fn delete_device(
	&self,
	user_id: OwnedUserId,
	device_id: OwnedDeviceId,
) -> Result {
	if !self.services.globals.user_is_local(&user_id) {
		return Err!("Cannot delete device of remote user");
	}

	self.services
		.users
		.remove_device(&user_id, &device_id)
		.await;

	write!(self, "User {user_id}'s device {device_id} removed.").await
}
