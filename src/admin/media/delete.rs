use ruma::OwnedMxcUri;
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn delete(&self, mxc: OwnedMxcUri) -> Result {
	self.services
		.media
		.delete(&mxc.as_str().try_into()?)
		.await?;

	self.write_str("Deleted the MXC from our database and on our filesystem.")
		.await
}
