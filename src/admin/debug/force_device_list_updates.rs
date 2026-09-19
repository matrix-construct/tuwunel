use futures::StreamExt;
use tuwunel_core::Result;
use tuwunel_service::users::DeviceListChange;

use crate::admin_command;

#[admin_command]
pub(super) async fn force_device_list_updates(&self) -> Result {
	self.services
		.users
		.stream()
		.for_each(|user_id| {
			self.services
				.users
				.mark_device_key_update(user_id, DeviceListChange::Resync)
		})
		.await;

	write!(self, "Marked all devices for all users as having new keys to update").await
}
