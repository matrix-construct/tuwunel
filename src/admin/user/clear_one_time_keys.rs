use tuwunel_core::{Result, utils::string::plural};

use crate::{admin_command, utils::parse_local_user_id};

#[admin_command]
pub(super) async fn clear_one_time_keys(&self, username: Option<String>) -> Result {
	let user_id = username
		.as_deref()
		.map(|username| parse_local_user_id(self.services, username))
		.transpose()?;

	let (keys, devices) = self
		.services
		.users
		.clear_one_time_keys(user_id.as_deref())
		.await?;

	let keys_noun = plural(keys, "one-time key", "one-time keys");
	let devices_noun = plural(devices, "device", "devices");

	write!(self, "Cleared {keys} {keys_noun} from {devices} {devices_noun}.").await
}
